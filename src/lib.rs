use std::{
    cmp::Reverse,
    collections::HashSet,
    fs::{File, OpenOptions},
    io::BufWriter,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use glob::glob;
use rayon::{
    iter::{IntoParallelRefIterator, ParallelIterator},
    slice::ParallelSliceMut,
};

use crate::{
    event::Event,
    formats::{
        Kind,
        rmp::{self, BinaryWriter},
    },
};

pub mod commands;
pub mod event;
pub mod formats;
pub mod matcher;
pub mod ui;

/// memory map `file`
/// # Panics
///
/// Will panic if statx or memory mapping fails, and if we can't set the `MADV_SEQUENTIAL` hint.
#[must_use]
pub fn mmap(file: &File) -> &'_ [u8] {
    #[expect(clippy::unwrap_used, reason = "panic if statx fails")]
    let len = file.metadata().unwrap().len();
    #[expect(clippy::expect_used, reason = "")]
    let len = libc::size_t::try_from(len).expect("casting file size to size_t is not truncating");
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert!(
            ptr != libc::MAP_FAILED,
            "{:?}",
            std::io::Error::last_os_error()
        );
        let r = libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
        assert!(r == 0, "{:?}", std::io::Error::last_os_error());
        std::slice::from_raw_parts(ptr as *const u8, len)
    }
}

/// discover all osh files of `kind` under `root`, recursively.
fn discover_files(root: &Path, kind: &formats::Kind) -> anyhow::Result<HashSet<PathBuf>> {
    let pattern = format!(
        "{}/**/*.{}",
        root.to_str()
            .ok_or(anyhow!("root path contains invalid chars"))?,
        kind.extension()
    );
    let files = match glob(&pattern) {
        Err(_) => unreachable!("pattern is valid"),
        Ok(matches) => matches
            .filter_map(Result::ok)
            .filter_map(|path| path.canonicalize().ok())
            .collect(),
    };
    Ok(files)
}

/// discover all parsable osh files under `~/.osh` for a specific format
///
/// # Errors
///
/// Will return an `Err` if `home_dir()` discovery fails.
pub fn osh_files(kind: &formats::Kind) -> anyhow::Result<HashSet<PathBuf>> {
    discover_files(&osh_dir()?, kind)
}

fn osh_dir() -> anyhow::Result<PathBuf> {
    let home_dir = home::home_dir().ok_or(anyhow!("no home directory"))?;
    Ok(home_dir.join(".osh"))
}

/// take an exclusive lock on `~/.osh/.lock` to serialise appends with rewrites of osh files.
/// The lock is released when the returned file is dropped.
///
/// # Errors
///
/// Will return an `Err` if the lock file can't be created or locked.
pub fn lock_osh_dir() -> anyhow::Result<File> {
    let dir = osh_dir()?;
    std::fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

/// remove all events with exactly `command` from all binary osh files in `~/.osh` and return
/// the number of removed events.
///
/// # Errors
///
/// Will return an `Err` if locking, parsing or rewriting any osh file fails.
pub fn remove_command(command: &str) -> anyhow::Result<usize> {
    let _lock = lock_osh_dir()?;
    osh_files(&Kind::Rmp)?
        .iter()
        .map(|path| remove_command_from_file(path, command))
        .sum()
}

/// rewrite `path` without the events matching `command`. The new content is written to a
/// temporary file which then replaces `path`, so readers never see a partially written file.
fn remove_command_from_file(path: &Path, command: &str) -> anyhow::Result<usize> {
    let data = std::fs::read(path)?;
    let events = rmp::load_osh_events(&data)?;
    let before = events.len();
    let kept: Vec<Event> = events
        .into_iter()
        .filter(|event| event.command != command)
        .collect();
    let removed = before - kept.len();
    if removed == 0 {
        return Ok(0);
    }

    let tmp_path = path.with_extension("bosh.tmp");
    let tmp_file = File::create(&tmp_path)?;
    tmp_file.set_permissions(std::fs::metadata(path)?.permissions())?;
    let mut writer = BinaryWriter::new(BufWriter::new(&tmp_file));
    for event in kept {
        writer.write(event)?;
    }
    writer.flush()?;
    drop(writer);
    tmp_file.sync_all()?;
    std::fs::rename(&tmp_path, path)?;
    Ok(removed)
}

/// load all binary osh files in `~/.osh` and return a merged and sorted vector of all events
///
/// # Errors
///
/// Will return an `Err` if collecting, memory mapping and parsing of osh files fails.
pub fn load_sorted() -> anyhow::Result<Vec<Event>> {
    let oshs = osh_files(&Kind::Rmp)?;
    let osh_files: Vec<File> = oshs
        .into_iter()
        .map(File::open)
        .collect::<Result<Vec<_>, _>>()?;
    let oshs_data: Vec<&[u8]> = osh_files.iter().map(mmap).collect();
    let all: Vec<Vec<Event>> = oshs_data
        .par_iter()
        .map(|data| rmp::load_osh_events(data))
        .collect::<Result<Vec<_>, _>>()?;

    let mut all_items: Vec<Event> = all.into_iter().flatten().collect();
    all_items.par_sort_unstable_by_key(|event| Reverse(event.endtime));
    Ok(all_items)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn mmap_reads_file_contents() {
        let mut file = tempfile::tempfile().unwrap();
        let data = b"hello mmap";
        file.write_all(data).unwrap();
        let mapped = mmap(&file);
        assert_eq!(mapped, data);
    }

    fn event(command: &str) -> Event {
        Event {
            timestamp_millis: 0,
            command: command.to_string(),
            endtime: 0,
            exit_code: 0,
            folder: String::new(),
            machine: String::new(),
            session: String::new(),
        }
    }

    #[test]
    fn remove_command_rewrites_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.bosh");
        let mut writer = BinaryWriter::new(File::create(&path).unwrap());
        for command in ["ls", "export TOKEN=secret", "pwd", "export TOKEN=secret"] {
            writer.write(event(command)).unwrap();
        }
        writer.flush().unwrap();

        let removed = remove_command_from_file(&path, "export TOKEN=secret").unwrap();
        assert_eq!(removed, 2);
        let commands: Vec<String> = rmp::load_osh_events(&std::fs::read(&path).unwrap())
            .unwrap()
            .into_iter()
            .map(|event| event.command)
            .collect();
        assert_eq!(commands, ["ls", "pwd"]);
        assert!(!path.with_extension("bosh.tmp").exists());
    }

    #[test]
    fn remove_command_skips_unmatched_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.bosh");
        let mut writer = BinaryWriter::new(File::create(&path).unwrap());
        writer.write(event("ls")).unwrap();
        writer.flush().unwrap();

        assert_eq!(remove_command_from_file(&path, "pwd").unwrap(), 0);
        assert!(!path.with_extension("bosh.tmp").exists());
    }

    #[test]
    fn discover_mixed_extensions() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("machine1")).unwrap();
        std::fs::File::create(dir.path().join("machine1/history.bosh")).unwrap();
        std::fs::File::create(dir.path().join("machine1/history.osh")).unwrap();

        let found = discover_files(dir.path(), &Kind::Rmp).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found.iter().all(|p| p.extension().unwrap() == "bosh"));
    }

    #[test]
    fn discover_empty_dir() {
        let dir = TempDir::new().unwrap();
        let found = discover_files(dir.path(), &Kind::Rmp).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn discover_nested() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::File::create(dir.path().join("a/one.bosh")).unwrap();
        std::fs::File::create(dir.path().join("a/b/two.bosh")).unwrap();

        let found = discover_files(dir.path(), &Kind::Rmp).unwrap();
        assert_eq!(found.len(), 2);
    }
}
