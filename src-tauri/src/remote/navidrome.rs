use super::scanner::song_from_remote_file;
use super::types::{RemoteFileEntry, RemoteSourceCredentials};
use super::webdav::shared_client;
use crate::music::types::Song;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use uuid::Uuid;

pub(super) fn base_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value.trim()).map_err(|_| "服务器地址无效")?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("请输入 Navidrome 服务根地址（可包含反向代理子路径）".into());
    }
    Ok(url)
}

fn endpoint(
    source: &RemoteSourceCredentials,
    method: &str,
    params: &[(&str, &str)],
) -> Result<Url, String> {
    let mut url = base_url(&source.base_url)?;
    let username = source
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("请输入用户名")?;
    let password = source
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("请输入密码")?;
    url.path_segments_mut()
        .map_err(|_| "服务器地址无效")?
        .pop_if_empty()
        .extend(["rest", method]);
    let salt = Uuid::new_v4().simple().to_string();
    let token = format!("{:x}", md5::compute(format!("{password}{salt}")));
    url.query_pairs_mut()
        .extend_pairs([
            ("u", username),
            ("s", salt.as_str()),
            ("t", token.as_str()),
            ("v", "1.16.1"),
            ("c", "LyciaPlayer"),
            ("f", "json"),
        ])
        .extend_pairs(params.iter().copied());
    Ok(url)
}

async fn request(
    source: &RemoteSourceCredentials,
    method: &str,
    params: &[(&str, &str)],
) -> Result<Value, String> {
    let url = endpoint(source, method, params)?;
    let response = shared_client()
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url().to_string())?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let mut data: Value =
        serde_json::from_slice(&bytes).map_err(|_| "服务器没有返回 Subsonic JSON")?;
    let body = data
        .get_mut("subsonic-response")
        .ok_or("服务器没有返回 Subsonic 响应")?;
    match body["status"].as_str() {
        Some("ok") => Ok(body.take()),
        _ => Err(format!(
            "Navidrome 错误 {}：{}",
            body["error"]["code"],
            body["error"]["message"].as_str().unwrap_or("请求失败")
        )),
    }
}

pub(super) async fn ping(source: &RemoteSourceCredentials) -> Result<(), String> {
    request(source, "ping.view", &[]).await.map(|_| ())
}

fn track_path(id: &str) -> String {
    format!("/tracks/{}", hex::encode(id))
}

fn decode_id(path: &str, prefix: &str) -> Result<String, String> {
    path.strip_prefix(prefix)
        .and_then(|v| hex::decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "无效的 Navidrome 标识".to_owned())
}

pub(super) fn stream_url(source: &RemoteSourceCredentials, path: &str) -> Result<Url, String> {
    let id = decode_id(path, "/tracks/")?;
    endpoint(source, "stream.view", &[("id", &id), ("format", "raw")])
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Track {
    id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    suffix: Option<String>,
    path: Option<String>,
    duration: Option<u32>,
    size: Option<u64>,
    bit_rate: Option<u32>,
    sampling_rate: Option<u32>,
    bit_depth: Option<u8>,
    track: Option<u32>,
    disc_number: Option<u32>,
    updated: Option<String>,
}

impl Track {
    fn file(&self) -> RemoteFileEntry {
        RemoteFileEntry {
            remote_path: track_path(&self.id),
            name: self
                .path
                .as_deref()
                .and_then(|p| p.rsplit(['/', '\\']).next())
                .unwrap_or(&self.title)
                .to_string(),
            size: self.size.unwrap_or(0),
            etag: Some(format!(
                "{}:{}",
                self.size.unwrap_or(0),
                self.updated.as_deref().unwrap_or_default()
            )),
            modified_at: self.updated.clone(),
            is_dir: false,
        }
    }

    fn indexed(self, source: &RemoteSourceCredentials) -> (RemoteFileEntry, Song) {
        let file = self.file();
        let mut song = song_from_remote_file(source, &file);
        song.title = self.title;
        song.artist = self.artist.unwrap_or_else(|| "未知歌手".into());
        song.artist_names = vec![song.artist.clone()];
        song.effective_artist_names = song.artist_names.clone();
        song.album = self.album.unwrap_or_else(|| "未知专辑".into());
        song.album_artist = self.album_artist.unwrap_or_else(|| song.artist.clone());
        song.album_key = format!(
            "{}::{}",
            song.album.to_lowercase(),
            song.album_artist.to_lowercase()
        );
        song.genre = self.genre;
        song.format = self.suffix.unwrap_or_default();
        song.duration = self.duration.unwrap_or(0);
        song.bitrate = self.bit_rate.unwrap_or(0);
        song.sample_rate = self.sampling_rate.unwrap_or(0);
        song.bit_depth = self.bit_depth;
        song.track_number = self.track.map(|n| n.to_string());
        song.disc_number = self.disc_number.map(|n| n.to_string());
        (file, song)
    }
}

fn entries<T: serde::de::DeserializeOwned>(
    body: &Value,
    section: &str,
    key: &str,
) -> Result<Vec<T>, String> {
    let object = body[section]
        .as_object()
        .ok_or_else(|| format!("Navidrome 响应缺少 {section}"))?;
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|e| format!("Navidrome {key}: {e}"))
        }
    }
}

pub(super) async fn catalog(
    source: &RemoteSourceCredentials,
) -> Result<(Vec<RemoteFileEntry>, Vec<Song>), String> {
    let mut files = Vec::new();
    let mut songs = Vec::new();
    let mut ids = HashSet::new();
    loop {
        let offset = files.len().to_string();
        let body = request(
            source,
            "search3.view",
            &[
                ("query", ""),
                ("artistCount", "0"),
                ("albumCount", "0"),
                ("songCount", "500"),
                ("songOffset", &offset),
            ],
        )
        .await?;
        let page: Vec<Track> = entries(&body, "searchResult3", "song")?;
        if page.is_empty() {
            break;
        }
        for track in page {
            if track.id.is_empty() || !ids.insert(track.id.clone()) {
                return Err("曲库分页出现空或重复歌曲 ID，请重新同步".into());
            }
            let (file, song) = track.indexed(source);
            files.push(file);
            songs.push(song);
        }
    }
    Ok((files, songs))
}

#[derive(Deserialize)]
struct Playlist {
    id: String,
    name: String,
}

pub(super) async fn browse(
    source: &RemoteSourceCredentials,
    path: &str,
) -> Result<Vec<RemoteFileEntry>, String> {
    if path == "/" {
        let body = request(source, "getPlaylists.view", &[]).await?;
        return Ok(entries::<Playlist>(&body, "playlists", "playlist")?
            .into_iter()
            .map(|p| RemoteFileEntry {
                remote_path: format!("/{}", hex::encode(p.id)),
                name: p.name,
                size: 0,
                etag: None,
                modified_at: None,
                is_dir: true,
            })
            .collect());
    }
    let id = decode_id(path, "/")?;
    let body = request(source, "getPlaylist.view", &[("id", &id)]).await?;
    Ok(entries::<Track>(&body, "playlist", "entry")?
        .into_iter()
        .map(|t| {
            let mut file = t.file();
            file.name = t.title;
            file
        })
        .collect())
}

pub(super) struct OfflineLibrary {
    pub(super) local_files: Vec<(String, super::local_library::LocalFile)>,
    playlists: Vec<(String, String)>,
}

impl OfflineLibrary {
    pub(super) async fn load(
        source: &RemoteSourceCredentials,
        files: &[RemoteFileEntry],
    ) -> Result<Self, String> {
        let root = browse(source, "/").await?;
        let mut playlists = Vec::with_capacity(root.len() + 1);
        for list in &root {
            let tracks = browse(source, &list.remote_path).await?;
            playlists.push((
                list.remote_path.clone(),
                serde_json::to_string(&tracks).map_err(|e| e.to_string())?,
            ));
        }
        playlists.push((
            "/".into(),
            serde_json::to_string(&root).map_err(|e| e.to_string())?,
        ));
        let local_files = super::local_library::match_library(source, files).await?;
        Ok(Self {
            local_files,
            playlists,
        })
    }

    pub(super) fn save(
        &self,
        conn: &rusqlite::Connection,
        source: &RemoteSourceCredentials,
    ) -> Result<(), String> {
        super::local_library::save(conn, source, &self.local_files)?;
        conn.execute(
            "DELETE FROM navidrome_playlists WHERE source_id=?1",
            [&source.id],
        )
        .map_err(|e| e.to_string())?;
        let mut insert = conn
            .prepare("INSERT INTO navidrome_playlists(source_id,path,entries) VALUES (?1,?2,?3)")
            .map_err(|e| e.to_string())?;
        for (path, entries) in &self.playlists {
            insert
                .execute(rusqlite::params![source.id, path, entries])
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

pub(super) fn cached_playlist(
    conn: &rusqlite::Connection,
    source_id: &str,
    path: &str,
) -> Result<Option<Vec<RemoteFileEntry>>, String> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT entries FROM navidrome_playlists WHERE source_id=?1 AND path=?2",
            [source_id, path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        .transpose()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source(base_url: String, password: String) -> RemoteSourceCredentials {
        RemoteSourceCredentials {
            id: "test-source".into(),
            name: "Test".into(),
            provider: "navidrome".into(),
            base_url,
            username: Some("fixture".into()),
            password: Some(password),
            root_path: "/".into(),
            enabled: true,
            last_sync_at: None,
            last_sync_error: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn auth_and_opaque_ids_survive_encoding() {
        let source = source("https://example.org/music/".into(), "p&?#".into());
        let id = "a/+彼?x";
        let url = stream_url(&source, &track_path(id)).unwrap();
        assert_eq!(url.path(), "/music/rest/stream.view");
        let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(params["id"], id);
        assert_eq!(
            params["t"],
            format!("{:x}", md5::compute(format!("p&?#{}", params["s"])))
        );
        assert!(!params.contains_key("p"));
        assert!(base_url("https://user:pass@example.org").is_err());
        assert!(entries::<Track>(&serde_json::json!({}), "searchResult3", "song").is_err());
    }

    // For a local server with the two tagged FLAC fixtures and the Reverse playlist.
    #[tokio::test]
    #[ignore]
    async fn real_navidrome_fixture() {
        let source = source(
            std::env::var("LYCIA_TEST_SERVER").unwrap(),
            std::env::var("LYCIA_TEST_PASSWORD").unwrap(),
        );
        ping(&source).await.unwrap();
        let (files, songs) = catalog(&source).await.unwrap();
        assert_eq!(files.len(), 2);
        assert!(songs
            .iter()
            .all(|s| s.artist == "Lycia Test" && s.duration == 2 && s.format == "flac"));
        let playlists = browse(&source, "/").await.unwrap();
        let list = playlists.iter().find(|p| p.name == "Reverse").unwrap();
        let tracks = browse(&source, &list.remote_path).await.unwrap();
        assert_eq!(
            tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["Fixture 2", "Fixture 1"]
        );
        assert!(tracks
            .iter()
            .all(|t| songs.iter().any(|s| s.path == t.remote_uri(&source.id))));
        let url = stream_url(&source, &tracks[0].remote_path).unwrap();
        let partial = shared_client()
            .get(url)
            .header("Range", "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(partial.status(), 206);
        assert_eq!(&partial.bytes().await.unwrap()[..], b"fLaC");
        let dst = std::env::temp_dir().join(format!("lycia-test-{}.flac", Uuid::new_v4()));
        super::super::webdav::download_file_to_path(
            &source,
            &tracks[0].remote_path,
            &dst,
            |_, _| {},
        )
        .await
        .unwrap();
        let audio = std::fs::read(&dst).unwrap();
        std::fs::remove_file(dst).unwrap();
        assert!(rodio::Decoder::new(Cursor::new(audio))
            .unwrap()
            .next()
            .is_some());
        // A local filename can differ from the virtual path returned by Navidrome.
        // Leave the server URL unusable during playback to exercise the offline resolver.
        let dir = std::env::temp_dir().join(format!("lycia-mirror-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let local = dir.join("renamed.flac");
        std::fs::write(
            &local,
            &shared_client()
                .get(stream_url(&source, &tracks[0].remote_path).unwrap())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap(),
        )
        .unwrap();
        let wrong = dir.join("same-size-wrong.flac");
        std::fs::write(
            &wrong,
            vec![0; std::fs::metadata(&local).unwrap().len() as usize],
        )
        .unwrap();
        let mut mirrored = source.clone();
        mirrored.root_path = dir.to_string_lossy().into_owned();
        let offline = OfflineLibrary::load(&mirrored, &files).await.unwrap();
        assert_eq!(offline.local_files.len(), 1);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::ensure_base_schema(&conn).unwrap();
        conn.execute("INSERT INTO remote_sources(id,name,provider,base_url,root_path,created_at,updated_at) VALUES (?1,'fixture','navidrome','http://127.0.0.1:1',?2,0,0)",rusqlite::params![mirrored.id,mirrored.root_path]).unwrap();
        offline.save(&conn, &mirrored).unwrap();
        let saved = cached_playlist(&conn, &mirrored.id, &list.remote_path)
            .unwrap()
            .unwrap();
        assert_eq!(saved[0].name, "Fixture 2");
        let uri = saved[0].remote_uri(&mirrored.id);
        let db = crate::database::DbState {
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let plan = super::super::cache::remote_playback_source(&db, &uri).unwrap();
        assert!(
            matches!(plan,super::super::cache::RemotePlaybackSource::Cached{path} if std::path::Path::new(&path)==local)
        );
        std::fs::write(&local, b"changed").unwrap();
        assert!(
            super::super::local_library::resolve(&db.conn.lock().unwrap(), &mirrored, &uri)
                .unwrap()
                .is_none()
        );
        std::fs::remove_file(local).unwrap();
        std::fs::remove_file(wrong).unwrap();
        std::fs::remove_dir(dir).unwrap();

        let mut invalid = source;
        invalid.password = Some("invalid".into());
        assert!(ping(&invalid).await.is_err());
    }
}
