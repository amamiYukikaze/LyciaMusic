use super::types::{RemoteFileEntry, RemoteSource, RemoteSourceCredentials, RemoteSourceInput};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const REMOTE_KEYRING_SERVICE: &str = "LyciaMusic WebDAV";

fn keyring_entry(source_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(REMOTE_KEYRING_SERVICE, source_id).map_err(|error| error.to_string())
}

fn read_password_from_keyring(source_id: &str) -> Option<String> {
    keyring_entry(source_id)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|password| !password.is_empty())
}

fn write_password_to_keyring(source_id: &str, password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Ok(());
    }
    keyring_entry(source_id)?
        .set_password(password)
        .map_err(|error| error.to_string())
}

fn write_password_to_keyring_verified(source_id: &str, password: &str) -> bool {
    write_password_to_keyring(source_id, password).is_ok()
        && read_password_from_keyring(source_id).as_deref() == Some(password)
}

fn delete_password_from_keyring(source_id: &str) {
    if let Ok(entry) = keyring_entry(source_id) {
        let _ = entry.delete_credential();
    }
}

fn attach_keyring_password(mut source: RemoteSourceCredentials) -> RemoteSourceCredentials {
    if source.password.is_none() {
        source.password = read_password_from_keyring(&source.id);
    }
    source
}

fn migrate_db_password_to_keyring(conn: &rusqlite::Connection, source: &RemoteSourceCredentials) {
    if source.provider == "webdav" && crate::data_storage::is_portable() { return; }
    let Some(password) = source.password.as_deref() else {
        return;
    };
    if write_password_to_keyring_verified(&source.id, password) {
        let _ = conn.execute(
            "UPDATE remote_sources SET password = NULL WHERE id = ?1",
            params![&source.id],
        );
    }
}

// Portable profiles must carry their WebDAV credentials to the next computer.
pub(crate) fn preserve_portable_credentials(conn: &rusqlite::Connection) -> Result<(), String> {
    if !crate::data_storage::is_portable() { return Ok(()); }
    let mut stmt = conn.prepare("SELECT id FROM remote_sources WHERE provider = 'webdav' AND password IS NULL").map_err(|e| e.to_string())?;
    let ids = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(|e| e.to_string())?;
    for id in ids {
        let id = id.map_err(|e| e.to_string())?;
        if let Some(password) = read_password_from_keyring(&id) {
            conn.execute("UPDATE remote_sources SET password = ?1 WHERE id = ?2", params![password, id]).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn normalize_root_path(value: Option<String>) -> String {
    let trimmed = value
        .unwrap_or_else(|| "/".to_string())
        .trim()
        .replace('\\', "/");
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed
    } else {
        format!("/{trimmed}")
    }
}

pub(crate) fn remote_path_from_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("remote://")?;
    let (source_id, path) = rest.split_once('/')?;
    Some((source_id.to_string(), format!("/{path}")))
}

pub(crate) fn list_sources(conn: &rusqlite::Connection) -> Result<Vec<RemoteSource>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, provider, base_url, username, root_path, enabled,
                    last_sync_at, last_sync_error, created_at, updated_at,
                    (SELECT count(*) FROM navidrome_local_files m WHERE m.source_id=remote_sources.id AND m.root=remote_sources.root_path)
             FROM remote_sources
             ORDER BY created_at DESC",
        )
        .map_err(|error| error.to_string())?;

    let rows = stmt
        .query_map([], |row| {
            Ok(RemoteSource {
                id: row.get(0)?,
                name: row.get(1)?,
                provider: row.get(2)?,
                base_url: row.get(3)?,
                username: row.get(4)?,
                root_path: row.get(5)?,
                enabled: row.get::<_, i64>(6)? != 0,
                last_sync_at: row.get(7)?,
                last_sync_error: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
                local_file_count: row.get(11)?,
            })
        })
        .map_err(|error| error.to_string())?;

    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

pub(crate) fn get_source(
    conn: &rusqlite::Connection,
    source_id: &str,
) -> Result<RemoteSourceCredentials, String> {
    let source = conn
        .query_row(
            "SELECT id, name, provider, base_url, username, password, root_path, enabled,
                last_sync_at, last_sync_error, created_at, updated_at
         FROM remote_sources
         WHERE id = ?1",
            params![source_id],
            |row| {
                Ok(RemoteSourceCredentials {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    provider: row.get(2)?,
                    base_url: row.get(3)?,
                    username: row.get(4)?,
                    password: row.get(5)?,
                    root_path: row.get(6)?,
                    enabled: row.get::<_, i64>(7)? != 0,
                    last_sync_at: row.get(8)?,
                    last_sync_error: row.get(9)?,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "远程音乐库不存在".to_string())?;

    migrate_db_password_to_keyring(conn, &source);
    Ok(attach_keyring_password(source))
}

pub(crate) fn get_source_for_remote_uri(
    conn: &rusqlite::Connection,
    uri: &str,
) -> Result<
    (
        RemoteSourceCredentials,
        String,
        Option<String>,
        Option<String>,
    ),
    String,
> {
    let (source_id, remote_path) =
        remote_path_from_uri(uri).ok_or_else(|| "无效的远程音乐路径".to_string())?;
    let source = get_source(conn, &source_id)?;
    let metadata = conn
        .query_row(
            "SELECT etag, remote_uri FROM remote_files WHERE source_id = ?1 AND remote_path = ?2",
            params![&source_id, &remote_path],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;

    let (etag, remote_uri) = metadata.unwrap_or((None, uri.to_string()));
    Ok((source, remote_path, etag, Some(remote_uri)))
}

pub(super) fn saved_password(input: &RemoteSourceInput, saved: &RemoteSourceCredentials) -> Option<String> {
    (saved.provider == input.provider && saved.base_url == input.base_url.trim().trim_end_matches('/')
        && saved.username == input.username).then(|| saved.password.clone()).flatten()
}

pub(crate) fn save_source(
    conn: &rusqlite::Connection,
    input: RemoteSourceInput,
) -> Result<RemoteSource, String> {
    if input.provider == "navidrome" {
        super::navidrome::base_url(&input.base_url)?;
    } else if input.provider != "webdav" {
        return Err("未知远程曲库类型".to_string());
    }

    let now = now_seconds();
    let id = input.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let root_path = if input.provider == "navidrome" {
        let root = input.root_path.as_deref().unwrap_or("/").trim();
        if root.is_empty() || root == "/" { "/".to_owned() }
        else if std::path::Path::new(root).is_absolute() { root.to_owned() }
        else { return Err("本地缓存库需要使用绝对路径".into()); }
    } else { normalize_root_path(input.root_path.clone()) };
    let name = input.name.trim();
    let base_url = input.base_url.trim().trim_end_matches('/');
    if name.is_empty() || base_url.is_empty() {
        return Err("名称和服务器地址不能为空".to_string());
    }

    let existing = get_source(conn, &id).ok();
    let password = input.password.clone()
        .or_else(|| existing.as_ref().and_then(|source| saved_password(&input, source)));
    if input.provider == "navidrome" && (input.username.as_deref().unwrap_or("").is_empty()
        || password.as_deref().unwrap_or("").is_empty()) {
        return Err("请输入 Navidrome 用户名和密码".into());
    }
    let created_at = existing.map(|source| source.created_at).unwrap_or(now);
    let db_password = match password.as_deref() {
        Some(password) if input.provider == "navidrome" => {
            if !write_password_to_keyring_verified(&id, password) {
                return Err("无法将 Navidrome 密码保存到 Windows 凭据管理器".into());
            }
            None
        }
        Some(password) if !crate::data_storage::is_portable() && write_password_to_keyring_verified(&id, password) => None,
        Some(password) => Some(password.to_string()),
        None => None,
    };

    conn.execute(
        "INSERT INTO remote_sources (
            id, name, provider, base_url, username, password, root_path, enabled,
            created_at, updated_at
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            provider = excluded.provider,
            base_url = excluded.base_url,
            username = excluded.username,
            password = excluded.password,
            root_path = excluded.root_path,
            updated_at = excluded.updated_at",
        params![
            &id,
            name,
            &input.provider,
            base_url,
            &input.username,
            &db_password,
            &root_path,
            created_at,
            now
        ],
    )
    .map_err(|error| error.to_string())?;

    Ok(get_source(conn, &id)?.into())
}

pub(crate) fn remove_source(
    conn: &mut rusqlite::Connection,
    source_id: &str,
) -> Result<(), String> {
    let provider: String = conn.query_row("SELECT provider FROM remote_sources WHERE id = ?1", [source_id], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM songs WHERE remote_source_id = ?1", [source_id])
        .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM remote_sources WHERE id = ?1", [source_id])
        .map_err(|error| error.to_string())?;
    crate::music::scan_settings::remove_settings(
        &tx,
        &crate::music::scan_settings::LibraryScanSource::Remote {
            source_id: source_id.to_string(),
        },
    )?;
    tx.execute(
        "DELETE FROM artists
         WHERE id NOT IN (SELECT DISTINCT artist_id FROM song_artists)",
        [],
    )
    .ok();
    tx.commit().map_err(|error| error.to_string())?;
    if provider == "navidrome" || !crate::data_storage::is_portable() {
        delete_password_from_keyring(source_id);
    }
    Ok(())
}

pub(crate) fn replace_remote_files(
    conn: &mut rusqlite::Connection,
    source_id: &str,
    files: &[RemoteFileEntry],
) -> Result<(), String> {
    let now = now_seconds();
    let tx = conn.savepoint().map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM remote_files WHERE source_id = ?1", [source_id])
        .map_err(|error| error.to_string())?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO remote_files (
                    source_id, remote_path, remote_uri, name, size, etag, modified_at, is_audio, indexed_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(|error| error.to_string())?;
        for file in files {
            stmt.execute(params![
                source_id,
                &file.remote_path,
                file.remote_uri(source_id),
                &file.name,
                file.size.min(i64::MAX as u64) as i64,
                &file.etag,
                &file.modified_at,
                if file.is_dir { 0 } else { 1 },
                now
            ])
            .map_err(|error| error.to_string())?;
        }
    }
    tx.commit().map_err(|error| error.to_string())
}

pub(crate) fn update_sync_status(
    conn: &rusqlite::Connection,
    source_id: &str,
    error: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE remote_sources
         SET last_sync_at = ?1, last_sync_error = ?2, updated_at = ?1
         WHERE id = ?3",
        params![now_seconds(), error, source_id],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn update_song_cache_path(
    conn: &rusqlite::Connection,
    remote_uri: &str,
    cache_path: &str,
) -> Result<(), String> {
    conn.execute(
        "UPDATE songs SET cache_path = ?1 WHERE path = ?2",
        params![cache_path, remote_uri],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn get_song_cache_path(
    conn: &rusqlite::Connection,
    remote_uri: &str,
) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT cache_path FROM songs WHERE path = ?1",
        params![remote_uri],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map_err(|error| error.to_string())
    .map(|value| value.flatten())
}
