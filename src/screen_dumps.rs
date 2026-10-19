//! Private screen-dump exports and a context-scoped local file browser.
//!
//! All operations are synchronous and belong on a filesystem worker. On Unix,
//! context directories and selected files are accessed relative to open directory
//! handles, so replacing a path with a symlink cannot redirect an operation.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, Metadata},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_DUMP_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_DUMP_ENTRIES: usize = 10_000;
static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat {
    Logs,
    Yaml,
    Text,
    Csv,
}

impl DumpFormat {
    const fn extension(self) -> &'static str {
        match self {
            Self::Logs => "log",
            Self::Yaml => "yaml",
            Self::Text => "txt",
            Self::Csv => "csv",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Screen dump must be a regular file",
            ));
        }
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenDumpEntry {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_unix: i64,
    directory: PathBuf,
    file_name: OsString,
    identity: FileIdentity,
}

impl ScreenDumpEntry {
    #[must_use]
    pub fn dir_display(&self) -> String {
        self.path
            .parent()
            .map_or_else(|| "-".to_owned(), |p| p.to_string_lossy().into_owned())
    }
}

/// The shared default used by log exports, the browser, and `kux info`.
///
/// # Errors
/// Returns an error when no absolute state/home directory can be resolved.
pub fn default_root() -> io::Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "No absolute state directory configured; set XDG_STATE_HOME or HOME",
            )
        })?;
    Ok(state.join("kux/screen-dumps"))
}

#[derive(Debug, Clone)]
pub struct ScreenDumpStore {
    root: PathBuf,
    relative: PathBuf,
    directory: PathBuf,
    legacy: Option<PathBuf>,
}

impl ScreenDumpStore {
    /// Select an export root and context. Construction does not create directories.
    ///
    /// # Errors
    /// Returns an error if a relative root cannot be resolved or a scope is too long.
    pub fn new(root: PathBuf, cluster: Option<&str>, context: &str) -> io::Result<Self> {
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        let relative = cluster
            .filter(|cluster| !cluster.is_empty())
            .map(encoded_component)
            .transpose()?
            .map_or_else(PathBuf::new, PathBuf::from)
            .join(encoded_component(context)?);
        let legacy = default_root()
            .ok()
            .filter(|default| *default == root)
            .map(|_| PathBuf::from(legacy_component(context)))
            .filter(|legacy| *legacy != relative);
        Ok(Self {
            directory: root.join(&relative),
            root,
            relative,
            legacy,
        })
    }

    /// # Errors
    /// Returns an error if the default root or context path cannot be resolved.
    pub fn from_default(cluster: Option<&str>, context: &str) -> io::Result<Self> {
        Self::new(default_root()?, cluster, context)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// List regular files, newest first. Missing directories produce an empty list.
    /// Existing default-root log exports are included from their legacy context path.
    ///
    /// # Errors
    /// Reports unreadable/unsafe directories, unreadable files, or an excessive listing.
    pub fn list(&self) -> io::Result<Vec<ScreenDumpEntry>> {
        let mut entries = Vec::new();
        for relative in std::iter::once(&self.relative).chain(self.legacy.iter()) {
            let directory = match Directory::open(&self.root, relative, false) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for name in directory.names()? {
                let file = match directory.read_file(&name) {
                    Ok(file) => file,
                    Err(error) if ignored_entry(&error) => continue,
                    Err(error) => return Err(error),
                };
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    continue;
                }
                let identity = FileIdentity::from_metadata(&metadata)?;
                entries.push(ScreenDumpEntry {
                    name: name
                        .to_string_lossy()
                        .chars()
                        .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch })
                        .collect(),
                    path: self.root.join(relative).join(&name),
                    size_bytes: identity.size,
                    modified_unix: unix_seconds(identity.modified),
                    directory: relative.clone(),
                    file_name: name,
                    identity,
                });
                if entries.len() > MAX_DUMP_ENTRIES {
                    return Err(io::Error::other("Screen-dump listing exceeds 10,000 files"));
                }
            }
        }
        entries.sort_unstable_by(|left, right| {
            right
                .identity
                .modified
                .cmp(&left.identity.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(entries)
    }

    /// Write a new private file without replacing any previous export.
    ///
    /// # Errors
    /// Reports unsafe paths, the 64 MiB limit, or filesystem write errors.
    pub fn save(&self, name: &str, body: &str, format: DumpFormat) -> io::Result<PathBuf> {
        check_size(body.len() as u64)?;
        self.save_with(name, format, |file| file.write_all(body.as_bytes()))
    }

    /// Stream into a new private file. A failed write or oversized result is removed.
    /// The callback is responsible for limiting any intermediate allocations.
    ///
    /// # Errors
    /// Reports unsafe paths, callback errors, the 64 MiB streaming limit, or filesystem errors.
    pub fn save_with(
        &self,
        name: &str,
        format: DumpFormat,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        let directory = Directory::open(&self.root, &self.relative, true)?;
        let sequence = EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = k8s_openapi::jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ");
        let filename = OsString::from(format!(
            "{}-{timestamp}-{}-{sequence}.{}",
            legacy_component(name),
            std::process::id(),
            format.extension()
        ));
        let mut file = BufWriter::with_capacity(64 * 1024, directory.create_file(&filename)?);
        let mut limited = LimitedWriter {
            file: &mut file,
            written: 0,
            exceeded: false,
        };
        let result = write(&mut limited).and_then(|()| {
            if limited.exceeded {
                check_size(MAX_DUMP_BYTES + 1)
            } else {
                limited.flush()
            }
        });
        drop(file);
        if let Err(error) = result {
            let cleanup = directory.remove_file(&filename);
            return Err(cleanup.err().map_or(error, |cleanup| {
                io::Error::other(format!(
                    "Export failed and its incomplete file could not be removed: {cleanup}"
                ))
            }));
        }
        Ok(self.directory.join(filename))
    }

    /// Read selected UTF-8 text without following symlinks or accepting a replaced file.
    ///
    /// # Errors
    /// Reports stale selections, invalid text, unsafe paths, or the 64 MiB limit.
    pub fn read(&self, entry: &ScreenDumpEntry) -> io::Result<String> {
        let (_, file) = self.open_selected(entry)?;
        check_size(file.metadata()?.len())?;
        let mut text = String::new();
        (&file).take(MAX_DUMP_BYTES + 1).read_to_string(&mut text)?;
        check_size(text.len() as u64)?;
        if FileIdentity::from_metadata(&file.metadata()?)? != entry.identity {
            return Err(io::Error::other(
                "Screen dump changed while being read; refresh and try again",
            ));
        }
        Ok(text)
    }

    /// Remove only a still-matching selected file. The caller enforces read-only policy.
    ///
    /// # Errors
    /// Reports stale selections, unsafe paths, or filesystem deletion errors.
    pub fn delete(&self, entry: &ScreenDumpEntry) -> io::Result<()> {
        let (directory, _) = self.open_selected(entry)?;
        directory.remove_file(&entry.file_name)
    }

    fn open_selected(&self, entry: &ScreenDumpEntry) -> io::Result<(Directory, File)> {
        if (entry.directory != self.relative && self.legacy.as_ref() != Some(&entry.directory))
            || entry.path != self.root.join(&entry.directory).join(&entry.file_name)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Screen dump belongs to another root or context",
            ));
        }
        let directory = Directory::open(&self.root, &entry.directory, false)?;
        let file = directory.read_file(&entry.file_name)?;
        if FileIdentity::from_metadata(&file.metadata()?)? != entry.identity {
            return Err(io::Error::other(
                "Screen dump changed since it was listed; refresh before opening or deleting it",
            ));
        }
        Ok((directory, file))
    }
}

struct LimitedWriter<'a, W: Write + ?Sized> {
    file: &'a mut W,
    written: u64,
    exceeded: bool,
}

impl<W: Write + ?Sized> Write for LimitedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > MAX_DUMP_BYTES.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(io::Error::other(
                "Screen dump exceeds the 64 MiB size limit",
            ));
        }
        let count = self.file.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn check_size(bytes: u64) -> io::Result<()> {
    if bytes > MAX_DUMP_BYTES {
        Err(io::Error::other(
            "Screen dump exceeds the 64 MiB size limit",
        ))
    } else {
        Ok(())
    }
}

fn encoded_component(value: &str) -> io::Result<String> {
    use std::fmt::Write as _;
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    if encoded.is_empty() {
        encoded.push_str("%00");
    }
    if encoded.len() > 240 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Screen-dump cluster/context name exceeds 240 encoded bytes",
        ));
    }
    Ok(encoded)
}

fn legacy_component(value: &str) -> String {
    let name: String = value
        .chars()
        .take(120)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "logs".to_owned()
    } else {
        name
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

fn ignored_entry(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    false
}

#[cfg(unix)]
struct Directory {
    file: std::os::fd::OwnedFd,
}

#[cfg(unix)]
impl Directory {
    fn open(root: &Path, relative: &Path, create: bool) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
        if create {
            std::fs::create_dir_all(root)?;
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut file = open(root, flags, Mode::empty())?;
        for component in relative.components() {
            let name = component.as_os_str();
            if create
                && let Err(error) = mkdirat(&file, name, Mode::from_raw_mode(0o700))
                && error != rustix::io::Errno::EXIST
            {
                return Err(error.into());
            }
            file = openat(&file, name, flags | OFlags::NOFOLLOW, Mode::empty())?;
        }
        Ok(Self { file })
    }

    fn names(&self) -> io::Result<Vec<OsString>> {
        use std::os::unix::ffi::OsStrExt;
        let mut entries = rustix::fs::Dir::read_from(&self.file)?;
        let mut names = Vec::new();
        let mut scanned = 0;
        while let Some(entry) = entries.read() {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if matches!(name.to_str(), Some("." | "..")) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_DUMP_ENTRIES {
                return Err(io::Error::other(
                    "Screen-dump directory exceeds 10,000 entries",
                ));
            }
            if matches!(
                entry.file_type(),
                rustix::fs::FileType::RegularFile | rustix::fs::FileType::Unknown
            ) {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }

    fn read_file(&self, name: &OsStr) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?
        .into())
    }

    fn create_file(&self, name: &OsStr) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(openat(
            &self.file,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )?
        .into())
    }

    fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        rustix::fs::unlinkat(&self.file, name, rustix::fs::AtFlags::empty()).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kux-dumps-{}-{}",
                std::process::id(),
                EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn store(&self, context: &str) -> ScreenDumpStore {
            ScreenDumpStore::new(self.0.join("dumps"), Some("cluster"), context).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exports_are_unique_private_and_scoped_without_traversal() {
        let fixture = Fixture::new();
        let store = fixture.store("../../context");
        assert_eq!(store.list().unwrap(), Vec::<ScreenDumpEntry>::new());
        assert!(!store.root.exists());
        let first = store.save("../../pod", "one\n", DumpFormat::Logs).unwrap();
        let second = store.save("../../pod", "two\n", DumpFormat::Logs).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(&store.directory));
        assert!(
            store
                .directory
                .ends_with("cluster/%2E%2E%2F%2E%2E%2Fcontext")
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one\n");
        assert_eq!(first.extension(), Some(OsStr::new("log")));
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size_bytes, 4);
        assert!(entries[0].modified_unix > 0);
        assert_eq!(store.read(&entries[0]).unwrap(), "two\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(store.directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn legacy_exports_remain_readable_and_deletable_alongside_scoped_exports() {
        let fixture = Fixture::new();
        let mut store = fixture.store("old/context");
        let legacy = PathBuf::from(legacy_component("old/context"));
        std::fs::create_dir_all(store.root.join(&legacy)).unwrap();
        std::fs::write(store.root.join(&legacy).join("old.log"), "legacy").unwrap();
        store.legacy = Some(legacy);
        store.save("new", "current", DumpFormat::Yaml).unwrap();
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        let old = entries
            .iter()
            .find(|entry| entry.name == "old.log")
            .unwrap();
        assert_eq!(store.read(old).unwrap(), "legacy");
        store.delete(old).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.list().unwrap()[0].path.starts_with(store.directory()));
    }

    #[test]
    fn selected_files_cannot_cross_contexts_or_accept_replacements() {
        let fixture = Fixture::new();
        let store = fixture.store("first");
        let path = store
            .save("selected", "original", DumpFormat::Text)
            .unwrap();
        let entry = store.list().unwrap().remove(0);
        assert!(fixture.store("second").read(&entry).is_err());
        let mut forged = entry.clone();
        forged.path = fixture.0.join("outside.txt");
        assert!(store.delete(&forged).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement content").unwrap();
        assert!(
            store
                .read(&entry)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        assert!(store.delete(&entry).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "replacement content"
        );
        store.delete(&store.list().unwrap().remove(0)).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn failed_streaming_exports_are_removed_and_csv_streams_are_preserved() {
        let fixture = Fixture::new();
        let store = fixture.store("context");
        let result = store.save_with("partial", DumpFormat::Csv, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("fixture writer failure"))
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fixture writer failure")
        );
        assert_eq!(store.list().unwrap(), Vec::<ScreenDumpEntry>::new());
        let path = store
            .save_with("pods", DumpFormat::Csv, |file| {
                file.write_all(b"NAME,NAMESPACE\n")?;
                file.write_all(b"api-0,prod\n")
            })
            .unwrap();
        assert_eq!(path.extension(), Some(OsStr::new("csv")));
        assert_eq!(
            store.read(&store.list().unwrap().remove(0)).unwrap(),
            "NAME,NAMESPACE\napi-0,prod\n"
        );
    }

    #[test]
    fn oversized_and_binary_reads_fail_and_writer_enforces_limit_before_io() {
        let fixture = Fixture::new();
        let store = fixture.store("context");
        let oversized = store.save("large", "", DumpFormat::Text).unwrap();
        File::options()
            .write(true)
            .open(&oversized)
            .unwrap()
            .set_len(MAX_DUMP_BYTES + 1)
            .unwrap();
        let entry = store.list().unwrap().remove(0);
        assert!(
            store
                .read(&entry)
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        store.delete(&entry).unwrap();
        let path = store.save("binary", "", DumpFormat::Text).unwrap();
        std::fs::write(&path, [0xff, 0xff]).unwrap();
        assert_eq!(
            store
                .read(&store.list().unwrap().remove(0))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let mut file = File::create(fixture.0.join("limited")).unwrap();
        let mut writer = LimitedWriter {
            file: &mut file,
            written: MAX_DUMP_BYTES - 2,
            exceeded: false,
        };
        assert!(writer.write_all(b"three").is_err());
        assert!(writer.exceeded);
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_files_and_replaced_context_directories_cannot_escape_store() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let store = fixture.store("context");
        let selected = store
            .save("selected", "original", DumpFormat::Text)
            .unwrap();
        let entry = store.list().unwrap().remove(0);
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("target"), "outside").unwrap();
        symlink(outside.join("target"), store.directory.join("link.txt")).unwrap();
        std::fs::create_dir(store.directory.join("nested")).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        std::fs::remove_file(&selected).unwrap();
        symlink(outside.join("target"), &selected).unwrap();
        assert!(store.read(&entry).is_err());
        assert!(store.delete(&entry).is_err());
        let moved = fixture.0.join("old-directory");
        std::fs::rename(store.directory(), &moved).unwrap();
        symlink(&outside, store.directory()).unwrap();
        assert!(store.list().is_err());
        assert!(store.save("escape", "bad", DumpFormat::Text).is_err());
        assert!(store.read(&entry).is_err());
        assert!(store.delete(&entry).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("target")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn excessive_scope_names_are_rejected_without_path_collisions() {
        let fixture = Fixture::new();
        assert!(ScreenDumpStore::new(fixture.0.clone(), None, &"a".repeat(241)).is_err());
        assert_ne!(
            encoded_component("a/b").unwrap(),
            encoded_component("a_b").unwrap()
        );
        assert_ne!(
            encoded_component(".").unwrap(),
            encoded_component("").unwrap()
        );
    }
}

#[cfg(not(unix))]
struct Directory {
    path: PathBuf,
}

#[cfg(not(unix))]
impl Directory {
    fn open(root: &Path, relative: &Path, create: bool) -> io::Result<Self> {
        let mut path = root.to_owned();
        if create {
            std::fs::create_dir_all(&path)?;
        }
        for component in relative.components() {
            path.push(component);
            if create
                && let Err(error) = std::fs::create_dir(&path)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error);
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Screen-dump directory must not be a symlink",
                ));
            }
        }
        Ok(Self { path })
    }
    fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)? {
            names.push(entry?.file_name());
            if names.len() > MAX_DUMP_ENTRIES {
                return Err(io::Error::other(
                    "Screen-dump directory exceeds 10,000 entries",
                ));
            }
        }
        Ok(names)
    }
    fn read_file(&self, name: &OsStr) -> io::Result<File> {
        let path = self.path.join(name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Screen dump must be a regular file",
            ));
        }
        File::open(path)
    }
    fn create_file(&self, name: &OsStr) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.join(name))
    }
    fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        std::fs::remove_file(self.path.join(name))
    }
}
