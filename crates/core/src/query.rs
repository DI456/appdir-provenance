///! Read-only views over the ledger.

use crate::db::{AppRow, DirRow};
use crate::types::DbPath;

#[derive(Debug, Clone)]
pub struct ExeQuery {
    pub exe_path: DbPath,
    pub app: Option<AppRow>,
    pub directories: Vec<DirRow>,
}

#[derive(Debug, Clone)]
pub struct QueryItem {
    pub directory: DirRow,
    pub app: Option<AppRow>,
}

impl ExeQuery {
    pub fn run(db: &crate::db::Db, exe_path: &str, include_stale: bool) -> crate::Result<ExeQuery> {
        let directories = db.dirs_of_exe(exe_path, include_stale)?;
        let app = match directories.first() {
            Some(d) => db.app_by_id(d.app_id)?,
            None => None,
        };
        Ok(ExeQuery {
            exe_path: crate::path::normalize(exe_path).unwrap_or_else(|| exe_path.to_string()),
            app,
            directories,
        })
    }

    pub fn found(&self) -> bool {
        !self.directories.is_empty()
    }
}

impl QueryItem {
    pub fn run(db: &crate::db::Db, dir_path: &str) -> crate::Result<Option<QueryItem>> {
        let d = db.dir_by_path(dir_path)?;
        Ok(d.map(|directory| QueryItem {
            app: db.app_by_id(directory.app_id).ok().flatten(),
            directory,
        }))
    }
}
