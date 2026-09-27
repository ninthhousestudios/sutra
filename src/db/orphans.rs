//! Production liveness for the orphans advisory (sutra/483): does any non-test
//! code reference a symbol? Liveness follows `Db::find_dead_symbols`, with
//! two changes the back-test measured (`docs/orphans-backtest.md`): a reference
//! only counts when it comes from non-test code, and structure keeps a symbol
//! live (a type through its members, a constructor through its class, one
//! alternative of a configurable import through another).

use rusqlite::{Connection, params};

use super::Db;
use crate::error::Result;

/// A symbol row as the orphans check reads it.
#[derive(Debug, Clone)]
pub struct SymbolSite {
    pub id: i64,
    pub qualified_name: String,
    pub short_name: String,
    pub kind: String,
    pub path: String,
    pub language: String,
    pub start_line: i64,
    pub flags: i64,
}

/// Whether production code reaches a symbol, and how many test references it
/// has (exercised, never called).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Liveness {
    pub live: bool,
    pub test_refs: usize,
}

const SITE_COLUMNS: &str = "s.id, s.qualified_name, s.short_name, s.kind, f.path, f.language, \
                            s.start_line, s.flags";

fn site(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolSite> {
    Ok(SymbolSite {
        id: row.get(0)?,
        qualified_name: row.get(1)?,
        short_name: row.get(2)?,
        kind: row.get(3)?,
        path: row.get(4)?,
        language: row.get(5)?,
        start_line: row.get(6)?,
        flags: row.get(7)?,
    })
}

/// A reference's source: its file, and whether it sits inside a test item
/// (`#[test]`, `#[cfg(test)]`, a Dart test). Bit 0x02 marks test only in Rust
/// and Dart (see `parser::flags_mark_test`).
const REF_SOURCE: &str = "SELECT rf.path, EXISTS (\
        SELECT 1 FROM symbols e \
        WHERE e.file_id = r.file_id AND e.start_line <= r.line AND e.end_line >= r.line \
          AND ((e.flags & 1) != 0 OR (rf.language IN ('rust', 'dart') AND (e.flags & 2) != 0))) \
     FROM refs r JOIN files rf ON rf.id = r.file_id";

/// `(production, test)` counts of the references `sql` selects (shaped as
/// [`REF_SOURCE`]), classifying a source file with `is_test_path`.
fn count_sources(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    is_test_path: &impl Fn(&str) -> bool,
) -> Result<(usize, usize)> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    let (mut production, mut test) = (0, 0);
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let in_test_item: bool = row.get(1)?;
        if in_test_item || is_test_path(&path) {
            test += 1;
        } else {
            production += 1;
        }
    }
    Ok((production, test))
}

fn ids(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params, |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

impl Db {
    /// Every symbol in `path` with this qualified name and kind (a Dart
    /// getter/setter pair or `#[cfg]` variants share one).
    pub fn symbols_defined_as(
        &self,
        path: &str,
        qualified_name: &str,
        kind: &str,
    ) -> Result<Vec<SymbolSite>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {SITE_COLUMNS} FROM symbols s JOIN files f ON f.id = s.file_id \
             WHERE f.path = ?1 AND s.qualified_name = ?2 AND s.kind = ?3 ORDER BY s.start_line"
        ))?;
        let rows = stmt.query_map(params![path, qualified_name, kind], site)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every symbol whose short name is `short_name`.
    pub fn symbols_named(&self, short_name: &str) -> Result<Vec<SymbolSite>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {SITE_COLUMNS} FROM symbols s JOIN files f ON f.id = s.file_id \
             WHERE s.short_name = ?1 ORDER BY f.path, s.start_line"
        ))?;
        let rows = stmt.query_map(params![short_name], site)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether non-test code reaches `sym`. Live when a production reference
    /// binds to it or to:
    ///
    /// - a same-file twin (same qualified name), as in `find_dead_symbols`;
    /// - any same-named method through an unqualified call, for a method
    ///   (no receiver types, so method calls bind by name);
    /// - one of its members, for a type (a Dart class used only through its
    ///   static members has refs on the members, none on the class);
    /// - its class or a sibling member, for a constructor (`Class::Class`,
    ///   which Dart uses for named and private constructors alike);
    /// - its twin in another URI of the same configurable import
    ///   (`import 'io.dart' if (dart.library.js_interop) 'web.dart'`): calls
    ///   bind to one alternative only.
    pub fn production_liveness(
        &self,
        sym: &SymbolSite,
        is_test_path: impl Fn(&str) -> bool,
    ) -> Result<Liveness> {
        let conn = self.conn.lock();
        let direct = |id: i64| {
            count_sources(
                &conn,
                &format!("{REF_SOURCE} WHERE r.target_symbol_id = ?1"),
                params![id],
                &is_test_path,
            )
        };
        let any_live = |ids: &[i64]| -> Result<bool> {
            for &id in ids {
                if direct(id)?.0 > 0 {
                    return Ok(true);
                }
            }
            Ok(false)
        };

        let (production, test_refs) = direct(sym.id)?;
        let done = |live| Ok(Liveness { live, test_refs });
        if production > 0 {
            return done(true);
        }
        let twins = ids(
            &conn,
            "SELECT t.id FROM symbols t JOIN symbols s ON s.id = ?1 \
             WHERE t.file_id = s.file_id AND t.qualified_name = s.qualified_name AND t.id != s.id",
            params![sym.id],
        )?;
        if any_live(&twins)? {
            return done(true);
        }
        if sym.kind == "method" {
            let (by_name, _) = count_sources(
                &conn,
                &format!(
                    "{REF_SOURCE} JOIN symbols m ON m.id = r.target_symbol_id \
                     WHERE m.short_name = ?1 AND m.kind = 'method' \
                       AND r.context_kind = 'call' AND r.qualifier IS NULL"
                ),
                params![sym.short_name],
                &is_test_path,
            )?;
            if by_name > 0 {
                return done(true);
            }
        }
        let members = |qualified_name: &str, except: i64| {
            ids(
                &conn,
                "SELECT id FROM symbols \
                 WHERE qualified_name = ?1 || '::' || short_name AND id != ?2",
                params![qualified_name, except],
            )
        };
        if any_live(&members(&sym.qualified_name, sym.id)?)? {
            return done(true);
        }
        if let Some((parent, own)) = sym.qualified_name.rsplit_once("::")
            && sym.kind == "method"
            && parent.rsplit("::").next() == Some(own)
        {
            let classes = ids(
                &conn,
                "SELECT id FROM symbols WHERE qualified_name = ?1 \
                 AND kind IN ('class', 'struct', 'enum', 'mixin')",
                params![parent],
            )?;
            if any_live(&classes)? || any_live(&members(parent, sym.id)?)? {
                return done(true);
            }
        }
        let alternatives = ids(
            &conn,
            "SELECT t.id FROM symbols s \
             JOIN imports i1 ON i1.resolved_file_id = s.file_id \
             JOIN imports i2 ON i2.file_id = i1.file_id AND i2.line = i1.line \
                            AND i2.resolved_file_id != s.file_id \
             JOIN symbols t ON t.file_id = i2.resolved_file_id \
                           AND t.qualified_name = s.qualified_name AND t.kind = s.kind \
             WHERE s.id = ?1",
            params![sym.id],
        )?;
        done(any_live(&alternatives)?)
    }
}
