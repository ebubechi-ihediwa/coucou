// Dropped files are copied into %LOCALAPPDATA%\Coucou\inbox so the original is
// never touched and the copy survives the drag source going away.
// The inbox is swept of anything older than a week, as on macOS.
//
// Two doors, and the page can only knock:
//   * `ingest` copies a file in. It accepts only a path Rust itself saw in an OS
//     drop event (`Grants`), so a page cannot ask for an arbitrary file.
//   * `read_confined` reads a file out for the chat. It reads only inside the
//     inbox, whatever path it is given, and never more than a size limit.
// Chat context therefore only ever carries bytes the user dropped and we copied.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::settings;

const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Largest file copied into the inbox. The copy streams, so this is about disk
/// and about not freezing the UI on a multi-gigabyte drop (`ingest_file` is a
/// plain command and runs on the main thread), not about memory.
pub const MAX_INGEST: u64 = 256 * 1024 * 1024;
/// Largest PDF or image read into memory for the chat. The API takes 32 MB per
/// request and base64 adds a third, so anything over ~24 MB is refused by the
/// API after we have already held it three times over (bytes, base64, JSON body).
/// 20 MiB leaves headroom and rejects nothing the API would have accepted.
pub const MAX_ATTACHMENT: u64 = 20 * 1024 * 1024;

/// How long a drop stays redeemable, and how many may wait at once.
const GRANT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_GRANTS: usize = 64;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DroppedFile {
    pub name: String,
    pub path: String,
    pub size: u64,
}

pub fn inbox_dir() -> PathBuf {
    settings::local_dir().join("inbox")
}

// ── What the user dropped ─────────────────────────────────────────────────────

/// Paths the OS handed to the island in a drop, recorded by Rust before the page
/// can react. `ingest_file` redeems one, once; anything else is refused, so a
/// compromised page cannot turn the command into "copy any file I name".
pub struct Grants {
    ttl: Duration,
    paths: Mutex<Vec<(PathBuf, Instant)>>,
}

impl Default for Grants {
    fn default() -> Self {
        Grants::with_ttl(GRANT_TTL)
    }
}

impl Grants {
    fn with_ttl(ttl: Duration) -> Self {
        Grants { ttl, paths: Mutex::new(Vec::new()) }
    }

    pub fn grant(&self, dropped: &[PathBuf]) {
        let now = Instant::now();
        let mut paths = self.paths.lock().unwrap();
        paths.retain(|(_, at)| now.duration_since(*at) < self.ttl);
        paths.extend(dropped.iter().map(|p| (p.clone(), now)));
        let excess = paths.len().saturating_sub(MAX_GRANTS);
        paths.drain(..excess);
    }

    /// True once per grant: redeeming a path uses it up.
    fn redeem(&self, path: &Path) -> bool {
        let now = Instant::now();
        let mut paths = self.paths.lock().unwrap();
        paths.retain(|(_, at)| now.duration_since(*at) < self.ttl);
        match paths.iter().position(|(p, _)| p == path) {
            Some(i) => {
                paths.remove(i);
                true
            }
            None => false,
        }
    }
}

/// What the page is told when it names a path nobody dropped. No path in it.
const NOT_DROPPED: &str = "Drop the file onto the island to share it.";

/// A short reason for the island's note view. Never the path, never the OS text.
fn describe(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => "That file is no longer there.".into(),
        io::ErrorKind::PermissionDenied => "Coucou isn't allowed to read that file.".into(),
        _ => "Couldn't read that file.".into(),
    }
}

fn too_large(limit: u64) -> String {
    format!("That file is too large to share (limit {} MB).", limit / (1024 * 1024))
}

pub fn ingest(grants: &Grants, source: &str) -> Result<DroppedFile, String> {
    let src = Path::new(source);
    if !grants.redeem(src) {
        return Err(NOT_DROPPED.into());
    }
    let dir = inbox_dir();
    crate::platform::ensure_private_dir(&settings::local_dir()).map_err(|e| describe(&e))?;
    copy_into(&dir, src, MAX_INGEST)
}

/// Copies `src` into `inbox` under a name that is not taken. Everything is judged
/// on the open file, not the path: what gets copied is what was checked.
fn copy_into(inbox: &Path, src: &Path, max: u64) -> Result<DroppedFile, String> {
    // Before opening: a folder has its own message, and a FIFO or device would
    // block the open (or never end) rather than fail.
    let before = std::fs::metadata(src).map_err(|e| describe(&e))?;
    if before.is_dir() {
        return Err("Folders can't be dropped yet.".into());
    }
    if !before.is_file() {
        return Err("Only regular files can be shared.".into());
    }
    let mut input = File::open(src).map_err(|e| describe(&e))?;
    let meta = input.metadata().map_err(|e| describe(&e))?;
    if !meta.is_file() {
        return Err("Only regular files can be shared.".into());
    }
    if meta.len() > max {
        return Err(too_large(max));
    }

    std::fs::create_dir_all(inbox).map_err(|e| describe(&e))?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let (mut output, dest) = create_unique(inbox, src, &name)?;

    // One byte past the limit tells a file that grew since we looked from one
    // that fits; the copy never writes more than that.
    let copied = io::copy(&mut (&mut input).take(max + 1), &mut output).and_then(|n| {
        output.flush()?;
        Ok(n)
    });
    drop(output);
    match copied {
        Ok(n) if n <= max => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&dest);
            return Err(too_large(max));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            return Err(describe(&e));
        }
    }
    sweep(inbox);

    Ok(DroppedFile {
        name,
        path: dest.to_string_lossy().to_string(),
        size: meta.len().min(max),
    })
}

/// Creates the destination with `create_new`, so it can neither overwrite a file
/// nor be a symlink someone planted in the inbox: the name is taken or it is ours.
fn create_unique(inbox: &Path, src: &Path, name: &str) -> Result<(File, PathBuf), String> {
    let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = src.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    for i in 1..1000 {
        let candidate = if i == 1 {
            inbox.join(name)
        } else {
            inbox.join(format!("{stem} ({i}){ext}"))
        };
        match File::options().write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(describe(&e)),
        }
    }
    Err("Too many files with that name in the inbox.".into())
}

/// Drops anything copied here more than a week ago. `copy_into` writes every copy
/// as a new file, so its modification time is when it landed, never the age of
/// whatever the user happened to drag in.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(copied) = meta.modified() else { continue };
        if now.duration_since(copied).map(|age| age > KEEP_FOR).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ── Reading a file out for the chat ───────────────────────────────────────────

/// Why a read was refused. `message` is what the island may show: no path, no OS text.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Not a file inside the inbox, by name or once the filesystem has resolved it.
    Outside,
    Missing,
    NotAFile,
    TooLarge(u64),
    Unreadable,
}

impl ReadError {
    pub fn message(&self) -> String {
        match self {
            ReadError::Outside => {
                "That file isn't in Coucou's inbox, so it can't be read. Drop it onto the island again.".into()
            }
            ReadError::Missing => "That file is no longer there.".into(),
            ReadError::NotAFile => "Only regular files can be shared.".into(),
            ReadError::TooLarge(limit) => too_large(*limit),
            ReadError::Unreadable => "Couldn't read that file.".into(),
        }
    }
}

/// Reads a file for the chat: inside `root` (the inbox), at most `max` bytes. The
/// root is a parameter so tests can use a scratch folder.
///
/// 1. By name, before the filesystem is touched: an absolute path strictly below
///    `root`, with no `..`, compared component by component (so `inbox-evil` is
///    not `inbox`) and in the exact form `root` has. That alone turns away
///    relative paths, other drives, `\\server\share` (opening one would start an
///    SMB login), `\\?\` and `\\.\` forms, and device names like `CON`.
/// 2. By the open file, not the name: the file is opened once and the place the
///    OS says that handle really is (symlinks, junctions and all resolved) must
///    be inside the real `root`. A link swapped in between steps 1 and 2 changes
///    what is opened, not what is checked, so there is no window to win.
/// 3. The size is read from that same handle, then the read itself is capped, so
///    a file that grows after the check still cannot exceed `max`.
pub(crate) fn read_confined(root: &Path, path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    let (file, _) = open_confined(root, path)?;
    let meta = file.metadata().map_err(|e| read_error(&e))?;
    if !meta.is_file() {
        return Err(ReadError::NotAFile);
    }
    if meta.len() > max {
        return Err(ReadError::TooLarge(max));
    }
    read_capped(file, max)
}

/// Steps 1 and 2 above, shared with everything else that must act only on a file
/// in the inbox: the open file and where the OS says it really is.
fn open_confined(root: &Path, path: &Path) -> Result<(File, PathBuf), ReadError> {
    if !named_inside(root, path) {
        return Err(ReadError::Outside);
    }
    let real_root = std::fs::canonicalize(root).map_err(|e| read_error(&e))?;
    let file = open_for_read(path).map_err(|e| read_error(&e))?;
    match opened_path(&file) {
        Ok(real) if real != real_root && real.starts_with(&real_root) => Ok((file, real)),
        Ok(_) => Err(ReadError::Outside),
        // Cannot say where the handle lives: not a reason to trust it.
        Err(_) => Err(ReadError::Unreadable),
    }
}

/// The real location of a regular file inside `root`, after the same checks a read
/// gets (name, open file, resolved location). For actions that hand a file to the
/// system rather than read it: they must be given *this* path, not the one asked
/// for, so a link swapped in afterwards changes nothing about what was checked.
pub(crate) fn confine_existing(root: &Path, path: &Path) -> Result<PathBuf, ReadError> {
    let (file, real) = open_confined(root, path)?;
    if !file.metadata().map_err(|e| read_error(&e))?.is_file() {
        return Err(ReadError::NotAFile);
    }
    Ok(real)
}

fn read_error(err: &io::Error) -> ReadError {
    match err.kind() {
        io::ErrorKind::NotFound => ReadError::Missing,
        _ => ReadError::Unreadable,
    }
}

/// Reads everything, but never accepts more than `max` bytes: one byte over means
/// the file outgrew its size, and nothing is returned.
fn read_capped(reader: impl Read, max: u64) -> Result<Vec<u8>, ReadError> {
    let mut bytes = Vec::new();
    reader
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ReadError::Unreadable)?;
    if bytes.len() as u64 > max {
        return Err(ReadError::TooLarge(max));
    }
    Ok(bytes)
}

/// Step 1 above.
fn named_inside(root: &Path, path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && path.starts_with(root)
        && path != root
        && !is_device_name(path)
}

/// `CON`, `NUL`, `COM1`… name devices on Windows, with or without an extension and
/// whatever folder precedes them. Opening one could block on a console.
#[cfg(windows)]
fn is_device_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let base = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit())
}

#[cfg(not(windows))]
fn is_device_name(_: &Path) -> bool {
    false
}

#[cfg(windows)]
fn open_for_read(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// O_NONBLOCK so that a FIFO planted in the inbox fails the type check below
/// instead of hanging the open.
#[cfg(target_os = "linux")]
fn open_for_read(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path)
}

/// Where the OS says this open file really is.
#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Where the OS says this open file really is: the final path of the handle, with
/// every symlink and junction resolved, in the same `\\?\C:\…` form that
/// `std::fs::canonicalize` gives the root.
#[cfg(windows)]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS, VOLUME_NAME_DOS,
    };

    let handle = HANDLE(file.as_raw_handle());
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `handle` is open for as long as `file` is borrowed, and `buf` is
        // a valid, writable slice whose length the call is told.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < buf.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buf[..len])));
        }
        // Too small: `len` is the size needed, terminator included.
        buf.resize(len, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch folder under the temp dir, removed on drop. Tests never touch
    /// the real inbox or any file that was not made here.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "coucou-files-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// The inbox stand-in, and a sibling folder that is outside it.
        fn inbox(&self) -> PathBuf {
            let dir = self.0.join("inbox");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn outside(&self) -> PathBuf {
            let dir = self.0.join("outside");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const BIG: u64 = 1 << 20;

    fn write(path: &Path, bytes: &[u8]) -> PathBuf {
        std::fs::write(path, bytes).unwrap();
        path.to_path_buf()
    }

    // ── Reading ──────────────────────────────────────────────────────────────

    #[test]
    fn a_file_inside_the_inbox_is_read() {
        let s = Scratch::new("read");
        let file = write(&s.inbox().join("note.txt"), b"hello");
        assert_eq!(read_confined(&s.inbox(), &file, BIG).unwrap(), b"hello");
        // Sub-folders of the inbox are inside it too.
        std::fs::create_dir_all(s.inbox().join("sub")).unwrap();
        let nested = write(&s.inbox().join("sub").join("a.txt"), b"nested");
        assert_eq!(read_confined(&s.inbox(), &nested, BIG).unwrap(), b"nested");
    }

    #[test]
    fn a_file_outside_the_inbox_is_refused() {
        let s = Scratch::new("outside");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        assert_eq!(read_confined(&s.inbox(), &secret, BIG), Err(ReadError::Outside));
    }

    #[test]
    fn dot_dot_cannot_climb_out() {
        let s = Scratch::new("dotdot");
        let secret = write(&s.0.join("secret.txt"), b"nope");
        let inbox = s.inbox();
        for climb in [
            inbox.join("..").join("secret.txt"),
            inbox.join("sub").join("..").join("..").join("secret.txt"),
            inbox.join("..").join("outside").join("..").join("secret.txt"),
        ] {
            assert!(secret.exists());
            assert_eq!(read_confined(&inbox, &climb, BIG), Err(ReadError::Outside), "{climb:?}");
        }
    }

    #[test]
    fn an_absolute_path_elsewhere_and_a_relative_one_are_refused() {
        let s = Scratch::new("absolute");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        assert!(secret.is_absolute());
        assert_eq!(read_confined(&s.inbox(), &secret, BIG), Err(ReadError::Outside));
        // The folder itself is not a file inside it, and a bare name is not absolute.
        assert_eq!(read_confined(&s.inbox(), &s.inbox(), BIG), Err(ReadError::Outside));
        assert_eq!(read_confined(&s.inbox(), Path::new("note.txt"), BIG), Err(ReadError::Outside));
        assert_eq!(read_confined(&s.inbox(), Path::new(""), BIG), Err(ReadError::Outside));
    }

    #[test]
    fn a_sibling_that_merely_starts_with_the_inbox_name_is_refused() {
        // The string-prefix trap: "…/inbox-evil/x" begins with "…/inbox".
        let s = Scratch::new("prefix");
        let evil = s.0.join("inbox-evil");
        std::fs::create_dir_all(&evil).unwrap();
        let file = write(&evil.join("x.txt"), b"nope");
        assert!(file.to_string_lossy().starts_with(&*s.inbox().to_string_lossy()));
        assert_eq!(read_confined(&s.inbox(), &file, BIG), Err(ReadError::Outside));
    }

    #[test]
    fn a_missing_file_is_reported_as_missing() {
        let s = Scratch::new("missing");
        let gone = s.inbox().join("gone.txt");
        assert_eq!(read_confined(&s.inbox(), &gone, BIG), Err(ReadError::Missing));
        // No inbox at all yet is the same: nothing there.
        let none = s.0.join("no-inbox");
        assert_eq!(read_confined(&none, &none.join("a.txt"), BIG), Err(ReadError::Missing));
    }

    #[test]
    fn a_folder_inside_the_inbox_is_not_a_file() {
        let s = Scratch::new("dir");
        let sub = s.inbox().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        // Windows will not even open a folder as a file; either way, no bytes.
        assert!(matches!(
            read_confined(&s.inbox(), &sub, BIG),
            Err(ReadError::NotAFile) | Err(ReadError::Unreadable)
        ));
    }

    // ── Links ────────────────────────────────────────────────────────────────

    /// A directory link that needs no privilege: a symlink on Linux, a junction on
    /// Windows (which is what a reparse-point escape through a folder looks like).
    fn link_dir(link: &Path, target: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(status.status.success(), "mklink /J failed: {status:?}");
        }
    }

    #[test]
    fn a_folder_link_inside_the_inbox_cannot_lead_out() {
        let s = Scratch::new("dirlink");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        let link = s.inbox().join("shortcut");
        link_dir(&link, &s.outside());
        // By name it is inside the inbox; on disk it is not.
        let through = link.join("secret.txt");
        assert!(named_inside(&s.inbox(), &through));
        assert!(secret.exists() && through.exists());
        assert_eq!(read_confined(&s.inbox(), &through, BIG), Err(ReadError::Outside));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_symlink_inside_the_inbox_cannot_lead_out() {
        let s = Scratch::new("filelink");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        let link = s.inbox().join("innocent.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert_eq!(read_confined(&s.inbox(), &link, BIG), Err(ReadError::Outside));
    }

    /// File symlinks on Windows need a privilege (or Developer Mode). Without it
    /// the case cannot be built here, and the junction test above stands in.
    #[cfg(windows)]
    #[test]
    fn a_file_symlink_inside_the_inbox_cannot_lead_out() {
        let s = Scratch::new("filelink");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        let link = s.inbox().join("innocent.txt");
        if let Err(e) = std::os::windows::fs::symlink_file(&secret, &link) {
            eprintln!("skipped: cannot create a file symlink here ({e})");
            return;
        }
        assert_eq!(read_confined(&s.inbox(), &link, BIG), Err(ReadError::Outside));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_stays_inside_the_inbox_is_fine() {
        let s = Scratch::new("inlink");
        let real = write(&s.inbox().join("real.txt"), b"inside");
        let link = s.inbox().join("alias.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(read_confined(&s.inbox(), &link, BIG).unwrap(), b"inside");
    }

    // ── Size ─────────────────────────────────────────────────────────────────

    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_read() {
        let s = Scratch::new("size");
        let at = write(&s.inbox().join("at.bin"), &[7u8; 10]);
        let over = write(&s.inbox().join("over.bin"), &[7u8; 11]);
        assert_eq!(read_confined(&s.inbox(), &at, 10).unwrap().len(), 10);
        assert_eq!(read_confined(&s.inbox(), &over, 10), Err(ReadError::TooLarge(10)));
        // A sparse file far past the limit is refused from its size alone.
        let huge = s.inbox().join("huge.bin");
        File::create(&huge).unwrap().set_len(MAX_ATTACHMENT + 1).unwrap();
        assert_eq!(
            read_confined(&s.inbox(), &huge, MAX_ATTACHMENT),
            Err(ReadError::TooLarge(MAX_ATTACHMENT))
        );
    }

    #[test]
    fn a_file_that_grows_after_the_size_check_is_still_capped() {
        // The size on the handle said "small"; the stream turns out endless.
        // Nothing more than the limit is ever held, and the answer is a refusal.
        assert_eq!(read_capped(io::repeat(b'x'), 1000), Err(ReadError::TooLarge(1000)));
        assert_eq!(read_capped(&[1u8, 2, 3][..], 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(read_capped(&[1u8, 2, 3, 4][..], 3), Err(ReadError::TooLarge(3)));
        assert_eq!(read_capped(io::empty(), 0).unwrap(), Vec::<u8>::new());
    }

    // ── Names (no filesystem involved) ───────────────────────────────────────

    #[test]
    fn what_counts_as_inside_is_decided_by_components() {
        #[cfg(unix)]
        {
            let root = Path::new("/home/u/.local/share/Coucou/inbox");
            assert!(named_inside(root, &root.join("a.txt")));
            assert!(named_inside(root, &root.join("sub/./a.txt")), "a `.` is the same place");
            assert!(named_inside(root, Path::new("/home/u/.local/share/Coucou/inbox//a.txt")));
            assert!(!named_inside(root, root));
            assert!(!named_inside(root, &root.join("../x")));
            assert!(!named_inside(root, Path::new("/home/u/.local/share/Coucou/inbox-evil/a")));
            assert!(!named_inside(root, Path::new("/etc/passwd")));
            assert!(!named_inside(root, Path::new("inbox/a.txt")));
        }
        #[cfg(windows)]
        {
            let root = Path::new(r"C:\Users\u\AppData\Local\Coucou\inbox");
            assert!(named_inside(root, &root.join("a.txt")));
            assert!(named_inside(root, Path::new(r"C:\Users\u\AppData\Local\Coucou\inbox\.\a.txt")));
            assert!(named_inside(root, Path::new(r"C:\Users\u\AppData\Local\Coucou\inbox\\a.txt")));
            assert!(!named_inside(root, root));
            assert!(!named_inside(root, &root.join(r"..\x")));
            assert!(!named_inside(root, Path::new(r"C:\Users\u\AppData\Local\Coucou\inbox-evil\a")));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_that_are_not_ours_are_refused_by_name_alone() {
        let root = Path::new(r"C:\Users\u\AppData\Local\Coucou\inbox");
        for path in [
            // UNC: opening it would start an SMB login before anything else.
            r"\\server\share\Coucou\inbox\a.txt",
            r"\\localhost\C$\Users\u\AppData\Local\Coucou\inbox\a.txt",
            // Another drive, and the drive-relative form.
            r"D:\Users\u\AppData\Local\Coucou\inbox\a.txt",
            r"C:inbox\a.txt",
            // Verbatim and device namespaces name the same place differently.
            r"\\?\C:\Users\u\AppData\Local\Coucou\inbox\a.txt",
            r"\\.\C:\Users\u\AppData\Local\Coucou\inbox\a.txt",
            r"\\?\UNC\server\share\a.txt",
            r"\\.\pipe\coucou-S-1-5-21-1",
            // Rooted but driveless.
            r"\Users\u\AppData\Local\Coucou\inbox\a.txt",
            // Reserved device names, in the inbox or not.
            r"C:\Users\u\AppData\Local\Coucou\inbox\CON",
            r"C:\Users\u\AppData\Local\Coucou\inbox\nul.txt",
            r"C:\Users\u\AppData\Local\Coucou\inbox\COM1",
            r"C:\Users\u\AppData\Local\Coucou\inbox\Lpt9.log",
        ] {
            assert!(!named_inside(root, Path::new(path)), "{path}");
            // And through the real entry point, which must not reach the disk.
            assert_eq!(read_confined(root, Path::new(path), BIG), Err(ReadError::Outside), "{path}");
        }
        // Not a device: only the exact reserved names are.
        assert!(!is_device_name(Path::new(r"C:\x\console.txt")));
        assert!(!is_device_name(Path::new(r"C:\x\COM10")));
    }

    // ── Dropping ─────────────────────────────────────────────────────────────

    #[test]
    fn ingest_copies_and_never_overwrites() {
        let s = Scratch::new("ingest");
        let inbox = s.inbox();
        let source = write(&s.outside().join("note.txt"), b"hello");

        let first = copy_into(&inbox, &source, BIG).unwrap();
        assert_eq!(first.name, "note.txt");
        assert_eq!(first.size, 5);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");

        // A second drop of the same name must not clobber the first copy.
        std::fs::write(&source, b"second").unwrap();
        let second = copy_into(&inbox, &source, BIG).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"second");

        // Folders are refused rather than silently ignored.
        assert_eq!(copy_into(&inbox, &s.outside(), BIG).unwrap_err(), "Folders can't be dropped yet.");

        // An ancient source must not arrive already older than the sweep window.
        let old_source = write(&s.outside().join("ancient.txt"), b"old");
        let long_ago = SystemTime::now() - KEEP_FOR - Duration::from_secs(60 * 60);
        File::options().write(true).open(&old_source).unwrap().set_modified(long_ago).unwrap();
        let aged = copy_into(&inbox, &old_source, BIG).unwrap();
        assert!(
            Path::new(&aged.path).exists(),
            "a file copied just now was swept as if it were a week old"
        );
    }

    #[test]
    fn a_dropped_file_can_then_be_read_for_the_chat() {
        // The whole legitimate path: the OS drop, the copy, the chat's read.
        let s = Scratch::new("flow");
        let grants = Grants::default();
        let source = write(&s.outside().join("brief.txt"), b"for the chat");
        grants.grant(std::slice::from_ref(&source));
        assert!(grants.redeem(&source));
        let copy = copy_into(&s.inbox(), &source, BIG).unwrap();
        assert_eq!(read_confined(&s.inbox(), Path::new(&copy.path), BIG).unwrap(), b"for the chat");
        // The original, outside the inbox, is still not readable by the chat.
        assert_eq!(read_confined(&s.inbox(), &source, BIG), Err(ReadError::Outside));
    }

    #[test]
    fn a_file_over_the_ingest_limit_is_refused_and_leaves_nothing_behind() {
        let s = Scratch::new("ingest-size");
        let source = write(&s.outside().join("big.bin"), &[1u8; 11]);
        let err = copy_into(&s.inbox(), &source, 10).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert_eq!(std::fs::read_dir(s.inbox()).unwrap().count(), 0);
        assert_eq!(copy_into(&s.inbox(), &write(&s.outside().join("ok.bin"), &[1u8; 10]), 10).unwrap().size, 10);
    }

    #[test]
    fn a_missing_source_is_an_error_not_a_panic() {
        let s = Scratch::new("ingest-missing");
        let err = copy_into(&s.inbox(), &s.outside().join("nope.txt"), BIG).unwrap_err();
        assert_eq!(err, "That file is no longer there.");
        assert!(!err.contains("nope"), "the message must not echo the path");
    }

    #[test]
    fn only_a_path_the_os_dropped_can_be_ingested() {
        let s = Scratch::new("grants");
        let dropped = write(&s.outside().join("dropped.txt"), b"x");
        let other = write(&s.outside().join("other.txt"), b"y");
        let grants = Grants::default();

        // Nobody dropped anything: the command is not a copy-any-file service.
        assert!(!grants.redeem(&dropped));

        grants.grant(std::slice::from_ref(&dropped));
        assert!(!grants.redeem(&other), "a path that was not in the drop");
        assert!(grants.redeem(&dropped));
        assert!(!grants.redeem(&dropped), "a drop is redeemed once");
    }

    #[test]
    fn the_public_ingest_refuses_a_path_nobody_dropped() {
        let s = Scratch::new("grants-api");
        let secret = write(&s.outside().join("secret.txt"), b"nope");
        let err = ingest(&Grants::default(), secret.to_str().unwrap()).unwrap_err();
        assert_eq!(err, NOT_DROPPED);
        assert!(!err.contains("secret"));
    }

    #[test]
    fn grants_expire_and_are_bounded() {
        let grants = Grants::with_ttl(Duration::ZERO);
        let p = PathBuf::from("a");
        grants.grant(std::slice::from_ref(&p));
        assert!(!grants.redeem(&p), "an expired drop cannot be redeemed");

        let grants = Grants::default();
        let many: Vec<PathBuf> = (0..MAX_GRANTS + 6).map(|i| PathBuf::from(format!("f{i}"))).collect();
        grants.grant(&many);
        assert!(!grants.redeem(Path::new("f0")), "the oldest are dropped past the cap");
        assert!(grants.redeem(Path::new(&format!("f{}", MAX_GRANTS + 5))));
    }
}
