//! Minimal read-only wrapper around SQLite shipped with Windows 10 and 11.
//!
//! Keep this intentionally narrow: the monitor only needs to retrieve one text
//! value from an application-owned database. Linking as a raw DLL import avoids
//! bundling SQLite or depending on a Windows SDK import library at build time.
//!
//! The database is always read where it is. It is never copied, because a copy
//! of another application's database is a copy of everything stored in it.

use std::ffi::{c_char, c_int, c_uchar, CStr, CString};
use std::fmt;
use std::path::Path;
use std::ptr;

const SQLITE_OK: c_int = 0;
const SQLITE_BUSY: c_int = 5;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READ_ONLY: c_int = 0x0000_0001;
const SQLITE_OPEN_URI: c_int = 0x0000_0040;
const BUSY_TIMEOUT_MS: c_int = 1_000;

/// How to open a database that its owning application may be writing to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadMode {
    /// Take SQLite's normal shared lock, so the value read is always
    /// consistent. Fails as busy while the owner holds a write lock.
    Locking,
    /// Open with `immutable=1`, so no lock is taken and none is waited for.
    /// A read that coincides with a write in progress may see a partly
    /// written page and fail, and in WAL mode it would miss changes not yet
    /// checkpointed; use it only after a `Locking` read reported busy.
    Immutable,
}

#[repr(C)]
struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct Sqlite3Stmt {
    _private: [u8; 0],
}

#[link(name = "winsqlite3", kind = "raw-dylib")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        database: *mut *mut Sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(database: *mut Sqlite3) -> c_int;
    fn sqlite3_errmsg(database: *mut Sqlite3) -> *const c_char;
    fn sqlite3_busy_timeout(database: *mut Sqlite3, milliseconds: c_int) -> c_int;
    fn sqlite3_prepare_v2(
        database: *mut Sqlite3,
        sql: *const c_char,
        sql_bytes: c_int,
        statement: *mut *mut Sqlite3Stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_bind_text(
        statement: *mut Sqlite3Stmt,
        index: c_int,
        value: *const c_char,
        value_bytes: c_int,
        destructor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    ) -> c_int;
    fn sqlite3_step(statement: *mut Sqlite3Stmt) -> c_int;
    fn sqlite3_column_text(statement: *mut Sqlite3Stmt, column: c_int) -> *const c_uchar;
    fn sqlite3_column_bytes(statement: *mut Sqlite3Stmt, column: c_int) -> c_int;
    fn sqlite3_finalize(statement: *mut Sqlite3Stmt) -> c_int;

    #[cfg(test)]
    fn sqlite3_exec(
        database: *mut Sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(
                *mut std::ffi::c_void,
                c_int,
                *mut *mut c_char,
                *mut *mut c_char,
            ) -> c_int,
        >,
        context: *mut std::ffi::c_void,
        error_message: *mut *mut c_char,
    ) -> c_int;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    message: String,
    /// SQLite's result code, or `None` for a failure detected before SQLite
    /// was called.
    code: Option<c_int>,
}

impl Error {
    fn local(message: &str) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    /// True when another connection's lock stopped the read.
    pub(crate) fn is_busy(&self) -> bool {
        // Mask off the extended-code bits in case they are ever enabled.
        self.code.is_some_and(|code| code & 0xff == SQLITE_BUSY)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

struct Connection {
    raw: *mut Sqlite3,
}

impl Connection {
    fn open_read_only(path: &Path, mode: ReadMode) -> Result<Self, Error> {
        let (filename, flags) = match mode {
            ReadMode::Locking => (
                path.as_os_str().as_encoded_bytes().to_vec(),
                SQLITE_OPEN_READ_ONLY,
            ),
            ReadMode::Immutable => (
                format!("{}?mode=ro&immutable=1", file_uri(path)?).into_bytes(),
                SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI,
            ),
        };
        let filename = CString::new(filename)
            .map_err(|_| Error::local("SQLite database path contains a NUL byte"))?;
        let mut raw = ptr::null_mut();
        let result = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut raw, flags, ptr::null()) };
        if result == SQLITE_OK && !raw.is_null() {
            let connection = Self { raw };
            let result = unsafe { sqlite3_busy_timeout(connection.raw, BUSY_TIMEOUT_MS) };
            if result != SQLITE_OK {
                return Err(error_message(
                    connection.raw,
                    "unable to set SQLite busy timeout",
                    result,
                ));
            }
            return Ok(connection);
        }

        let error = error_message(raw, "unable to open SQLite database", result);
        if !raw.is_null() {
            unsafe {
                let _ = sqlite3_close(raw);
            }
        }
        Err(error)
    }

    fn prepare(&self, sql: &str) -> Result<Statement<'_>, Error> {
        let sql =
            CString::new(sql).map_err(|_| Error::local("SQLite statement contains a NUL byte"))?;
        let mut raw = ptr::null_mut();
        let result =
            unsafe { sqlite3_prepare_v2(self.raw, sql.as_ptr(), -1, &mut raw, ptr::null_mut()) };
        if result != SQLITE_OK || raw.is_null() {
            return Err(error_message(
                self.raw,
                "unable to prepare SQLite statement",
                result,
            ));
        }
        Ok(Statement {
            raw,
            connection: self,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            let _ = sqlite3_close(self.raw);
        }
    }
}

struct Statement<'connection> {
    raw: *mut Sqlite3Stmt,
    connection: &'connection Connection,
}

impl Statement<'_> {
    fn bind_text(&mut self, index: c_int, value: &CStr) -> Result<(), Error> {
        let value_bytes = c_int::try_from(value.to_bytes().len())
            .map_err(|_| Error::local("SQLite parameter is too large"))?;
        // SQLITE_STATIC is safe here because the caller keeps `value` alive
        // until after sqlite3_step returns.
        let result =
            unsafe { sqlite3_bind_text(self.raw, index, value.as_ptr(), value_bytes, None) };
        if result == SQLITE_OK {
            Ok(())
        } else {
            Err(error_message(
                self.connection.raw,
                "unable to bind SQLite parameter",
                result,
            ))
        }
    }

    fn optional_text(&mut self, column: c_int) -> Result<Option<String>, Error> {
        match unsafe { sqlite3_step(self.raw) } {
            SQLITE_DONE => Ok(None),
            SQLITE_ROW => {
                let text = unsafe { sqlite3_column_text(self.raw, column) };
                if text.is_null() {
                    return Ok(None);
                }
                let bytes = unsafe { sqlite3_column_bytes(self.raw, column) };
                let bytes = usize::try_from(bytes)
                    .map_err(|_| Error::local("SQLite returned an invalid text length"))?;
                let value = unsafe { std::slice::from_raw_parts(text, bytes) };
                String::from_utf8(value.to_vec())
                    .map(Some)
                    .map_err(|_| Error::local("SQLite returned text that is not UTF-8"))
            }
            result => Err(error_message(
                self.connection.raw,
                "unable to read SQLite row",
                result,
            )),
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = sqlite3_finalize(self.raw);
        }
    }
}

fn error_message(database: *mut Sqlite3, context: &str, result: c_int) -> Error {
    let detail = if database.is_null() {
        None
    } else {
        let message = unsafe { sqlite3_errmsg(database) };
        (!message.is_null()).then(|| unsafe { CStr::from_ptr(message) }.to_string_lossy())
    };
    let message = match detail {
        Some(detail) => format!("{context} ({result}): {detail}"),
        None => format!("{context} ({result})"),
    };
    Error {
        message,
        code: Some(result),
    }
}

/// Build an SQLite URI filename (<https://sqlite.org/uri.html>) for a Windows
/// path. Backslashes become `/`, and every byte other than unreserved ASCII,
/// `/` and `:` is percent-encoded, so a `#` or `%` in a folder name cannot be
/// taken for the start of a fragment or an escape, and non-ASCII names pass
/// through as percent-encoded UTF-8.
fn file_uri(path: &Path) -> Result<String, Error> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::local("SQLite database path is not valid Unicode"))?;
    if text.contains('\0') {
        return Err(Error::local("SQLite database path contains a NUL byte"));
    }
    // A `\\?\` prefix only means something with backslashes, which the URI
    // cannot keep, so rewrite it to the ordinary form of the same path.
    let text = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(text).to_string(),
    };
    // `C:\x` becomes `file:///C:/x`; a share `\\server\x` keeps its two
    // leading slashes after the empty authority: `file:////server/x`.
    let mut uri = String::from(if text.starts_with(['\\', '/']) {
        "file://"
    } else {
        "file:///"
    });
    for byte in text.bytes() {
        match byte {
            b'\\' => uri.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                uri.push(char::from(byte))
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(uri)
}

/// Query column zero from the first row of a read-only, one-parameter query.
pub(crate) fn query_optional_text(
    path: &Path,
    mode: ReadMode,
    sql: &str,
    parameter: &str,
) -> Result<Option<String>, Error> {
    let parameter = CString::new(parameter)
        .map_err(|_| Error::local("SQLite parameter contains a NUL byte"))?;
    let connection = Connection::open_read_only(path, mode)?;
    let mut statement = connection.prepare(sql)?;
    statement.bind_text(1, &parameter)?;
    statement.optional_text(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SQLITE_OPEN_READ_WRITE: c_int = 0x0000_0002;
    const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;

    const QUERY: &str = "SELECT value FROM ItemTable WHERE key = ?1";
    const KEY: &str = "cursorAuth/accessToken";

    #[test]
    fn reads_an_optional_text_value_through_windows_sqlite() {
        let folder = scratch_folder("plain");
        let path = folder.join("state.vscdb");
        create_database(&path);

        for mode in [ReadMode::Locking, ReadMode::Immutable] {
            assert_eq!(
                query_optional_text(&path, mode, QUERY, KEY).unwrap(),
                Some("test-token".into()),
                "{mode:?}"
            );
            assert_eq!(
                query_optional_text(&path, mode, QUERY, "missing").unwrap(),
                None,
                "{mode:?}"
            );
        }

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn an_immutable_read_is_not_blocked_by_the_owners_write_lock() {
        let folder = scratch_folder("locked");
        let path = folder.join("state.vscdb");
        create_database(&path);
        let owner = open_writable(&path);
        exec(owner, "BEGIN EXCLUSIVE;");

        let locked = query_optional_text(&path, ReadMode::Locking, QUERY, KEY).unwrap_err();
        assert!(locked.is_busy(), "{locked}");
        assert_eq!(
            query_optional_text(&path, ReadMode::Immutable, QUERY, KEY).unwrap(),
            Some("test-token".into())
        );

        exec(owner, "ROLLBACK;");
        assert_eq!(unsafe { sqlite3_close(owner) }, SQLITE_OK);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn reading_leaves_nothing_beside_the_database() {
        let folder = scratch_folder("tidy");
        let path = folder.join("state.vscdb");
        create_database(&path);

        for mode in [ReadMode::Locking, ReadMode::Immutable] {
            query_optional_text(&path, mode, QUERY, KEY).unwrap();
        }

        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.vscdb"]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn an_immutable_read_finds_a_database_whose_path_needs_escaping() {
        let folder = scratch_folder("Ünïcødé #1 100% a&b=c");
        let path = folder.join("state.vscdb");
        create_database(&path);

        assert_eq!(
            query_optional_text(&path, ReadMode::Immutable, QUERY, KEY).unwrap(),
            Some("test-token".into())
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn file_uris_follow_the_sqlite_rules_for_windows_paths() {
        let uri = |path: &str| file_uri(Path::new(path)).unwrap();
        assert_eq!(
            uri(r"C:\Users\me\AppData\Roaming\Cursor\User\globalStorage\state.vscdb"),
            "file:///C:/Users/me/AppData/Roaming/Cursor/User/globalStorage/state.vscdb"
        );
        assert_eq!(
            uri(r"C:\a b\#1\100%\é"),
            "file:///C:/a%20b/%231/100%25/%C3%A9"
        );
        assert_eq!(uri(r"\\server\share\x.db"), "file:////server/share/x.db");
        assert_eq!(uri(r"\\?\C:\x.db"), "file:///C:/x.db");
        assert_eq!(
            uri(r"\\?\UNC\server\share\x.db"),
            "file:////server/share/x.db"
        );
    }

    fn scratch_folder(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!(
            "ccum-pro-winsqlite-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn open_writable(path: &Path) -> *mut Sqlite3 {
        let filename = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut database = ptr::null_mut();
        let result = unsafe {
            sqlite3_open_v2(
                filename.as_ptr(),
                &mut database,
                SQLITE_OPEN_READ_WRITE | SQLITE_OPEN_CREATE,
                ptr::null(),
            )
        };
        assert_eq!(result, SQLITE_OK);
        database
    }

    fn exec(database: *mut Sqlite3, sql: &str) {
        let sql = CString::new(sql).unwrap();
        let result = unsafe {
            sqlite3_exec(
                database,
                sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(result, SQLITE_OK);
    }

    fn create_database(path: &Path) {
        let database = open_writable(path);
        exec(
            database,
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);\
             INSERT INTO ItemTable VALUES ('cursorAuth/accessToken', 'test-token');",
        );
        assert_eq!(unsafe { sqlite3_close(database) }, SQLITE_OK);
    }
}
