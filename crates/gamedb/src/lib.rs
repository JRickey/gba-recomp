//! Reader for the shipped `gamedb.sqlite`: function boundaries for every
//! mapped GBA cartridge, keyed by ROM sha256. The launcher hashes the
//! user's image, looks up its functions here, and seeds a 0-interpreter
//! recompile from them.
//!
//! Implements [`recomp_core::LabelSource`] by feeding each row through
//! [`recomp_core::labels::Labels::push_function`] — the same per-record
//! logic the TOML map parser uses — so a DB-driven build seeds identically
//! to a file-driven one.
//!
//! Schema (built by gba-map-atlas `scripts/build_gamedb.py`):
//!
//! ```sql
//! functions(sha256 TEXT, address INTEGER, end INTEGER, mode TEXT, name TEXT)
//! gamedb_meta(key TEXT, value TEXT)   -- schema_version, counts
//! ```

use std::path::Path;

use recomp_core::{labels::Labels, LabelSource};
use rusqlite::{Connection, OpenFlags};

/// An open, read-only handle to a `gamedb.sqlite`.
pub struct GameDb {
    conn: Connection,
}

impl GameDb {
    /// Open the database read-only.
    pub fn open(path: &Path) -> Result<GameDb, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(GameDb { conn })
    }

    /// How many functions the database records for `sha` (0 = the game is
    /// not mapped here). Lets a caller distinguish "no coverage" from "an
    /// empty but present map" before committing to a build.
    pub fn function_count(&self, sha: &str) -> Result<usize, String> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM functions WHERE sha256 = ?1",
                [sha],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(|e| e.to_string())
    }
}

impl LabelSource for GameDb {
    fn labels(&self, sha: &str, rom_len: usize) -> Result<Labels, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT address, end, mode, name FROM functions WHERE sha256 = ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([sha], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u32,
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        let mut labels = Labels::default();
        for row in rows {
            let (addr, end, mode, name) = row.map_err(|e| e.to_string())?;
            let thumb = u32::from(mode == "thumb");
            labels.push_function(addr, thumb, end, name.as_deref(), rom_len);
        }
        Ok(labels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(rows: &[(&str, i64, Option<i64>, &str, Option<&str>)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("gamedb-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("g{}.sqlite", rows.len()));
        let _ = std::fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE functions (sha256 TEXT, address INTEGER, end INTEGER, mode TEXT, name TEXT);",
        )
        .unwrap();
        for (sha, addr, end, mode, name) in rows {
            conn.execute(
                "INSERT INTO functions VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![sha, addr, end, mode, name],
            )
            .unwrap();
        }
        p
    }

    #[test]
    fn reconstructs_labels_and_filters_by_sha() {
        let p = temp_db(&[
            ("aa", 0x0800_0100, Some(0x0800_0140), "thumb", Some("foo")),
            ("aa", 0x0800_0000, Some(0x0800_0004), "arm", None),
            ("bb", 0x0800_0200, None, "thumb", None), // different game — must not leak
        ]);
        let db = GameDb::open(&p).unwrap();
        let l = db.labels("aa", 0x40_0000).unwrap();

        // thumb sets the low bit in the key; arm does not.
        assert_eq!(l.rom.len(), 2);
        assert!(l.rom.contains(&(0x0800_0100 | 1)));
        assert!(l.rom.contains(&0x0800_0000));
        // enrichment carried through.
        assert_eq!(
            l.names.get(&(0x0800_0100 | 1)).map(String::as_str),
            Some("foo")
        );
        assert_eq!(l.ends.get(&(0x0800_0100 | 1)), Some(&0x0800_0140));
        // counts and sha isolation.
        assert_eq!(db.function_count("aa").unwrap(), 2);
        assert_eq!(db.function_count("bb").unwrap(), 1);
        assert_eq!(db.function_count("zz").unwrap(), 0);
    }
}
