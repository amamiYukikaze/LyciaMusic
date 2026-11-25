use super::navidrome::stream_url;
use super::types::{RemoteFileEntry, RemoteSourceCredentials};
use super::webdav::shared_client;
use reqwest::header::{CONTENT_RANGE, RANGE};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::task::JoinSet;
use walkdir::WalkDir;

const SAMPLE: u64 = 4096;
const PARALLEL_READS: usize = 6;

#[derive(Clone)]
pub(super) struct LocalFile {
    path: PathBuf,
    size: u64,
    modified: i64,
}

impl LocalFile {
    fn inspect(path: PathBuf) -> Option<Self> {
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let modified = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos()
            .try_into()
            .ok()?;
        Some(Self {
            path,
            size: meta.len(),
            modified,
        })
    }
}

fn ranges(size: u64) -> Vec<(u64, u64)> {
    if size <= SAMPLE * 3 {
        return vec![(0, size)];
    }
    vec![
        (0, SAMPLE),
        ((size - SAMPLE) / 2, SAMPLE),
        (size - SAMPLE, SAMPLE),
    ]
}

fn local_sample(file: &LocalFile, ranges: &[(u64, u64)]) -> std::io::Result<Vec<u8>> {
    let mut reader = File::open(&file.path)?;
    let mut data = Vec::new();
    for &(offset, length) in ranges {
        reader.seek(SeekFrom::Start(offset))?;
        let start = data.len();
        data.resize(start + length as usize, 0);
        reader.read_exact(&mut data[start..])?;
    }
    Ok(data)
}

async fn match_file(
    source: RemoteSourceCredentials,
    entry: RemoteFileEntry,
    candidates: Vec<LocalFile>,
) -> Result<Option<(String, LocalFile)>, String> {
    let ranges = ranges(entry.size);
    let mut sample = Vec::new();
    for &(offset, length) in &ranges {
        let end = offset + length - 1;
        let url = stream_url(&source, &entry.remote_path)?;
        let mut response = shared_client()
            .get(url)
            .header(RANGE, format!("bytes={offset}-{end}"))
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let expected = format!("bytes {offset}-{end}/{}", entry.size);
            if response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|h| h.to_str().ok())
                != Some(expected.as_str())
            {
                return Ok(None);
            }
        } else if !(response.status() == reqwest::StatusCode::OK
            && offset == 0
            && length == entry.size
            && response.content_length() == Some(length))
        {
            return Ok(None);
        }
        let initial = sample.len();
        while let Some(bytes) = response
            .chunk()
            .await
            .map_err(|e| e.without_url().to_string())?
        {
            if sample.len() - initial + bytes.len() > length as usize {
                return Ok(None);
            }
            sample.extend_from_slice(&bytes);
        }
        if sample.len() - initial != length as usize {
            return Ok(None);
        }
    }
    let mut matches = tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .filter(|f| local_sample(f, &ranges).is_ok_and(|v| v == sample))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    if matches.len() > 1 {
        // Equal-sized, sample-identical files can contain different recordings between the sampled regions.
        let url = stream_url(&source, &entry.remote_path)?;
        let mut response = shared_client()
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.without_url().to_string())?;
        let mut hash = Sha256::new();
        let mut read = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| e.without_url().to_string())?
        {
            read += chunk.len() as u64;
            if read > entry.size {
                return Ok(None);
            }
            hash.update(&chunk);
        }
        if read != entry.size {
            return Ok(None);
        }
        let digest = hash.finalize();
        matches = tokio::task::spawn_blocking(move || {
            matches.retain(|f| {
                let Ok(mut reader) = File::open(&f.path) else {
                    return false;
                };
                let mut hash = Sha256::new();
                std::io::copy(&mut reader, &mut hash).is_ok() && hash.finalize() == digest
            });
            matches.sort_by(|l, r| l.path.cmp(&r.path));
            matches
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(matches
        .into_iter()
        .next()
        .map(|f| (entry.remote_uri(&source.id), f)))
}

pub(super) async fn match_library(
    source: &RemoteSourceCredentials,
    files: &[RemoteFileEntry],
) -> Result<Vec<(String, LocalFile)>, String> {
    if source.root_path == "/" || source.root_path.is_empty() {
        return Ok(Vec::new());
    }
    let sizes = files.iter().map(|f| f.size).collect::<HashSet<_>>();
    let root = PathBuf::from(&source.root_path);
    let by_size = tokio::task::spawn_blocking(move || {
        let mut index: HashMap<u64, Vec<LocalFile>> = HashMap::new();
        if !root.is_absolute() {
            return index;
        }
        for item in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !item.file_type().is_file() {
                continue;
            }
            if let Some(file) = LocalFile::inspect(item.into_path()) {
                if file.size > 0 && sizes.contains(&file.size) {
                    index.entry(file.size).or_default().push(file);
                }
            }
        }
        index
    })
    .await
    .map_err(|e| e.to_string())?;
    let mut pending = JoinSet::new();
    let mut matches = Vec::new();
    for file in files {
        let Some(candidates) = by_size.get(&file.size) else {
            continue;
        };
        pending.spawn(match_file(source.clone(), file.clone(), candidates.clone()));
        if pending.len() >= PARALLEL_READS {
            if let Some(found) = pending
                .join_next()
                .await
                .unwrap()
                .map_err(|e| e.to_string())??
            {
                matches.push(found);
            }
        }
    }
    while let Some(result) = pending.join_next().await {
        if let Some(found) = result.map_err(|e| e.to_string())?? {
            matches.push(found);
        }
    }
    Ok(matches)
}

pub(super) fn save(
    conn: &Connection,
    source: &RemoteSourceCredentials,
    files: &[(String, LocalFile)],
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM navidrome_local_files WHERE source_id=?1",
        [&source.id],
    )
    .map_err(|e| e.to_string())?;
    let mut insert=conn.prepare("INSERT INTO navidrome_local_files (source_id,root,uri,path,size,modified) VALUES (?1,?2,?3,?4,?5,?6)").map_err(|e| e.to_string())?;
    for (uri, file) in files {
        insert
            .execute(params![
                source.id,
                source.root_path,
                uri,
                file.path.to_string_lossy(),
                i64::try_from(file.size).map_err(|_| "音频文件过大")?,
                file.modified
            ])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) fn resolve(
    conn: &Connection,
    source: &RemoteSourceCredentials,
    uri: &str,
) -> Result<Option<String>, String> {
    if source.provider != "navidrome" || source.root_path == "/" {
        return Ok(None);
    }
    let stored: Option<(String, i64, i64)> = conn
        .query_row(
            "SELECT path,size,modified FROM navidrome_local_files
         WHERE source_id=?1 AND root=?2 AND uri=?3",
            params![source.id, source.root_path, uri],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(stored.and_then(|(path, size, modified)| {
        let local = LocalFile::inspect(Path::new(&path).to_owned())?;
        (Some(local.size) == u64::try_from(size).ok() && local.modified == modified).then_some(path)
    }))
}
