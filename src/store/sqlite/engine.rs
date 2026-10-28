//! The [`MemoryStore`](crate::store::MemoryStore) operations shared by the SQLite-dialect stores.
//!
//! A store provides its transactions ([`Engine`]); these functions clean and check the input
//! first, so a rejected call never opens a write transaction, then run the shared statements.

use std::time::Instant;

use super::sql::Sql;
use super::{graph, listing, notes, schema, search, settings};
use crate::store::extensions::{
    BACKFILL_BUDGET, BACKFILL_PAGE, Columns, unavailable_setting, warn_backfill, warn_unavailable,
};
use crate::store::listing::excluded_json;
use crate::store::{Edge, Forget, Hit, Link, Neighborhood, Note, NoteDraft, NotePatch, Saved, SearchQuery, clean};
use crate::store::{Forgotten, NoteListQuery, NotePage, StoreError, Target, View};

/// The transactions of one SQLite-dialect store.
pub(crate) trait Engine {
    /// Runs `op` in a write transaction that takes the write lock first (`BEGIN IMMEDIATE`),
    /// committed when `op` succeeds and rolled back when it fails.
    fn write<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;

    /// As [`Self::write`] in a single attempt: when another writer holds the lock, it fails at once
    /// instead of waiting for it. For the counters, which are dropped rather than waited for.
    fn write_once<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.write(op)
    }

    /// Runs `op` without a transaction (each statement on its own).
    fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError>;

    /// True when the database has the additive schema extensions (see [`install`]): without them
    /// no statement may name `notes_fts_en`, `settings` or `notes.lang`.
    fn extensions(&self) -> bool;

    /// True when the database has the usefulness counters of a note (see [`install`]); apart from
    /// [`Self::extensions`], which a read-only role can have without them.
    fn usage(&self) -> bool;

    /// The optional columns of a note the database has.
    fn columns(&self) -> Columns {
        Columns {
            lang: self.extensions(),
            usage: self.usage(),
        }
    }
}

pub(crate) fn remember(engine: &mut impl Engine, target: &Target, draft: &NoteDraft) -> Result<Saved, StoreError> {
    let draft = clean::draft(draft)?;
    let extensions = engine.extensions();
    engine.write(|db| notes::remember(db, target, &draft, extensions))
}

pub(crate) fn update(engine: &mut impl Engine, view: &View, id: i64, patch: &NotePatch) -> Result<Saved, StoreError> {
    let patch = clean::patch(patch)?;
    let extensions = engine.extensions();
    engine.write(|db| notes::update(db, view, id, &patch, extensions))
}

/// A write: reading counts a hit on each note.
pub(crate) fn get(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    let columns = engine.columns();
    engine.write(|db| notes::get(db, view, ids, true, columns))
}

/// A read: counts no hit.
pub(crate) fn peek(engine: &mut impl Engine, view: &View, ids: &[i64]) -> Result<Vec<Note>, StoreError> {
    let columns = engine.columns();
    engine.read(|db| notes::get(db, view, ids, false, columns))
}

pub(crate) fn list_notes(engine: &mut impl Engine, query: &NoteListQuery) -> Result<NotePage, StoreError> {
    let columns = engine.columns();
    engine.read(|db| listing::list(db, query, columns))
}

pub(crate) fn search(engine: &mut impl Engine, query: &SearchQuery) -> Result<Vec<Hit>, StoreError> {
    let extensions = engine.extensions();
    engine.read(|db| search::search(db, query, extensions))
}

/// Counts a time shown on each note of `ids`; nothing without the usage counters. A single
/// attempt: it never waits for the write lock ([`Engine::write_once`]).
pub(crate) fn record_shown(engine: &mut impl Engine, ids: &[i64]) -> Result<(), StoreError> {
    let Some(ids) = notes::ids_json(ids).filter(|_| engine.usage()) else {
        return Ok(());
    };

    engine.write_once(|db| notes::record_shown(db, &ids))
}

/// Counts a time found useful, at `at`, on each note of `ids`; nothing without the usage counters.
pub(crate) fn record_useful(engine: &mut impl Engine, ids: &[i64], at: i64) -> Result<(), StoreError> {
    let Some(ids) = notes::ids_json(ids).filter(|_| engine.usage()) else {
        return Ok(());
    };

    engine.write_once(|db| notes::record_useful(db, &ids, at))
}

pub(crate) fn similar(engine: &mut impl Engine, view: &View, id: i64, limit: usize) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::similar(db, view, id, limit))
}

pub(crate) fn similar_text(
    engine: &mut impl Engine,
    view: &View,
    text: (&str, &str),
    limit: usize,
) -> Result<Vec<Hit>, StoreError> {
    engine.read(|db| search::resembling(db, view, text, None, limit))
}

pub(crate) fn link(engine: &mut impl Engine, target: &Target, link: &Link) -> Result<Edge, StoreError> {
    let link = clean::link(link)?;
    engine.write(|db| graph::link(db, target, &link))
}

pub(crate) fn neighbors(
    engine: &mut impl Engine,
    view: &View,
    entity: &str,
    depth: u8,
    limit: usize,
) -> Result<Neighborhood, StoreError> {
    engine.read(|db| graph::neighbors(db, view, entity, depth, limit))
}

pub(crate) fn forget(
    engine: &mut impl Engine,
    view: &View,
    target: &Target,
    what: &Forget,
) -> Result<usize, StoreError> {
    let what = clean::forget(what)?;

    engine.write(|db| match &what {
        Forget::Note(id) => notes::forget(db, view, *id),
        Forget::Entity(name) => graph::forget_entity(db, target, name),
        Forget::Relation { from, relation, to } => graph::forget_relation(db, target, from, relation, to),
    })
}

pub(crate) fn forget_notes(engine: &mut impl Engine, view: &View, exclude: &[i64]) -> Result<Forgotten, StoreError> {
    let exclude = excluded_json(exclude)?;
    engine.write(|db| notes::forget_all(db, view, &exclude))
}

/// The note titled like `title` in the scope `target`, read exactly and without counting a hit.
pub(crate) fn find_by_title(
    engine: &mut impl Engine,
    target: &Target,
    title: &str,
) -> Result<Option<Note>, StoreError> {
    let title = clean::title(title)?;
    let columns = engine.columns();
    engine.read(|db| notes::find_by_title(db, target, &title, columns))
}

/// What [`install`] found in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Installed {
    /// The English index, `lang` and the settings (see [`Engine::extensions`]).
    pub extensions: bool,
    /// The usefulness counters (see [`Engine::usage`]).
    pub usage: bool,
}

/// Adds the schema extensions the database lacks (see [`schema::ensure_extensions`]) and reports
/// which it has now. A database that refuses them is used with what it has, with one line on
/// stderr.
pub(crate) fn install(engine: &mut impl Engine) -> Installed {
    let present = engine.read(|db| Ok((schema::extensions_present(db)?, schema::usage_present(db)?)));

    let (extensions, usage) = match present {
        Ok(present) => present,
        Err(error) => {
            warn_unavailable(&error);
            return Installed {
                extensions: false,
                usage: false,
            };
        }
    };

    if extensions && usage {
        return Installed { extensions, usage };
    }

    match engine.write(schema::ensure_extensions) {
        Ok(()) => Installed {
            extensions: true,
            usage: true,
        },
        Err(error) => {
            warn_unavailable(&error);
            Installed { extensions, usage }
        }
    }
}

/// Fills `lang` of the notes an older server wrote, a page at a time and for at most
/// [`BACKFILL_BUDGET`]; what is left waits for the next open. A failure is reported once.
pub(crate) fn backfill(engine: &mut impl Engine) {
    if !engine.extensions() {
        return;
    }

    let deadline = Instant::now() + BACKFILL_BUDGET;

    while Instant::now() < deadline {
        let step = match engine.read(notes::has_pending_lang) {
            Ok(true) => engine.write(|db| notes::fill_lang(db, BACKFILL_PAGE)),
            Ok(false) => return,
            Err(error) => Err(error),
        };

        match step {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => return warn_backfill(&error),
        }
    }
}

/// The English notes setting; on without extensions.
pub(crate) fn english_notes(engine: &mut impl Engine) -> Result<bool, StoreError> {
    if !engine.extensions() {
        return Ok(true);
    }

    engine.read(settings::english_notes)
}

pub(crate) fn set_english_notes(engine: &mut impl Engine, on: bool) -> Result<(), StoreError> {
    if !engine.extensions() {
        return Err(unavailable_setting());
    }

    engine.write(|db| settings::set_english_notes(db, on))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::super::local::Local;
    use super::super::schema::{V1, V2, V3, V4, V5};
    use super::*;
    use crate::store::{NoteKind, NoteSort, UsefulFilter};
    use crate::support::lang::Lang;

    /// A database a role without the right to change the schema reads: the write fails.
    struct ReadOnlyRole {
        conn: Connection,
        installed: Installed,
    }

    impl Engine for ReadOnlyRole {
        fn write<T>(&mut self, _op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
            Err(StoreError::Backend("attempt to write a readonly database".to_owned()))
        }

        fn read<T>(&mut self, op: impl FnOnce(&mut dyn Sql) -> Result<T, StoreError>) -> Result<T, StoreError> {
            op(&mut Local(&self.conn))
        }

        fn extensions(&self) -> bool {
            self.installed.extensions
        }

        fn usage(&self) -> bool {
            self.installed.usage
        }
    }

    /// The database an upgraded server left (the language, the English index, the settings) with
    /// the usage counters taken out, holding two notes with a language.
    fn without_counters() -> ReadOnlyRole {
        let conn = Connection::open_in_memory().expect("memory database");

        for sql in [V1, V2, V3, V4, V5] {
            conn.execute_batch(sql).expect("schema");
        }

        schema::ensure_extensions(&mut Local(&conn)).expect("extensions");
        conn.execute_batch(
            "ALTER TABLE notes DROP COLUMN shown;
             ALTER TABLE notes DROP COLUMN useful;
             ALTER TABLE notes DROP COLUMN last_useful_at;
             INSERT INTO notes (kind, title, title_norm, body, created_at, updated_at, lang)
                 VALUES ('note', 'First', 'first', 'The first note', 1, 1, 'en'),
                        ('note', 'Second', 'second', 'The second note', 2, 2, 'en');",
        )
        .expect("notes");

        ReadOnlyRole {
            conn,
            installed: Installed {
                extensions: false,
                usage: false,
            },
        }
    }

    fn everything() -> NoteListQuery {
        NoteListQuery {
            view: View {
                repo: None,
                global: true,
            },
            text: String::new(),
            kind: None,
            scope: None,
            place: None,
            entity: None,
            lang: None,
            useful: None,
            sort: NoteSort::Updated,
            descending: true,
            offset: 0,
            limit: 50,
            exclude: Vec::new(),
        }
    }

    #[test]
    fn a_role_that_cannot_add_the_counters_keeps_the_language_and_shows_no_counters() {
        let mut role = without_counters();
        role.installed = install(&mut role);

        assert_eq!(
            role.installed,
            Installed {
                extensions: true,
                usage: false
            }
        );

        let page = list_notes(&mut role, &everything()).expect("list");
        assert_eq!(page.rows.len(), 2);

        for row in &page.rows {
            assert_eq!((row.usage, row.lang), (None, Some(Lang::En)), "{}", row.title);
        }

        let first = peek(&mut role, &everything().view, &[page.rows[0].id]).expect("peek");
        assert_eq!((first[0].usage, first[0].kind), (None, NoteKind::Note));

        // No note has counters, so none is useful or never useful, and the sorts fall back to ids.
        for useful in [UsefulFilter::Ever, UsefulFilter::Never] {
            let filtered = list_notes(
                &mut role,
                &NoteListQuery {
                    useful: Some(useful),
                    ..everything()
                },
            )
            .expect("list");
            assert_eq!((filtered.total, filtered.rows.len()), (0, 0), "{useful:?}");
        }

        for sort in [NoteSort::Useful, NoteSort::LastUseful] {
            let sorted = list_notes(&mut role, &NoteListQuery { sort, ..everything() }).expect("list");
            let ids: Vec<i64> = sorted.rows.iter().map(|row| row.id).collect();
            assert_eq!(ids, vec![2, 1], "{sort:?}");
        }

        // Counting has nowhere to go: it succeeds and writes nothing.
        record_shown(&mut role, &[1, 2]).expect("shown");
        record_useful(&mut role, &[1], 10).expect("useful");
    }
}
