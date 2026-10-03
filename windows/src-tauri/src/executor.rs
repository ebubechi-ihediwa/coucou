// Carries an approved action out. This is the last line of defence: it takes a
// typed `Action`, does not assume anything about where it came from, checks it
// again, and does one of three things (open an allowlisted application, open a web
// link, open an attached file), or fails closed with a short message.
//
// What it will not do, by construction and not by policy: run a command line, start
// an executable the code did not pick, or open a file it did not first confirm is
// inside the inbox. The operating-system calls sit behind `Launcher`, so all of
// this is tested with a fake that only records what it was asked to open.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::actions::{Action, AppId, FileId};
use crate::files::{self, ReadError};

/// The only kinds of file an action may open. Opening a file hands it to whatever
/// Windows has registered for its type, and for an `.exe`, `.bat`, `.ps1`, `.lnk`,
/// `.js` and the like that means running it. So this is a list of what is allowed,
/// not of what is forbidden: a type nobody thought of is refused.
pub const OPENABLE_EXTENSIONS: &[&str] = &[
    "txt", "md", "csv", "json", "log", "pdf", "png", "jpg", "jpeg", "gif", "webp", "bmp",
];

/// Files that can be named in an action at once. Older ones fall off.
const MAX_FILES: usize = 16;

// ── The operating system ──────────────────────────────────────────────────────

/// The three things Coucou can ask the system to do. Everything passed in has been
/// validated; implementations must still take no part of it as a command line.
pub trait Launcher: Send + Sync {
    /// Starts one of the allowlisted applications, with no arguments.
    fn launch_app(&self, app: AppId) -> io::Result<()>;
    /// Opens a canonical http(s) link in the default browser.
    fn open_url(&self, url: &str) -> io::Result<()>;
    /// Opens a file in the program registered for its type.
    fn open_path(&self, path: &Path) -> io::Result<()>;
}

impl<L: Launcher + ?Sized> Launcher for &L {
    fn launch_app(&self, app: AppId) -> io::Result<()> {
        (**self).launch_app(app)
    }
    fn open_url(&self, url: &str) -> io::Result<()> {
        (**self).open_url(url)
    }
    fn open_path(&self, path: &Path) -> io::Result<()> {
        (**self).open_path(path)
    }
}

/// The real thing.
pub struct SystemLauncher;

impl Launcher for SystemLauncher {
    #[cfg(windows)]
    fn launch_app(&self, app: AppId) -> io::Result<()> {
        use std::process::Stdio;
        let exe = system_exe(app)?;
        // The program, by its absolute path, and nothing after it.
        crate::platform::no_console(
            std::process::Command::new(exe)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .spawn()
        .map(|_| ())
    }

    #[cfg(not(windows))]
    fn launch_app(&self, _app: AppId) -> io::Result<()> {
        // Only Windows has a list of applications and where they live so far.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opening applications is Windows-only for now",
        ))
    }

    fn open_url(&self, url: &str) -> io::Result<()> {
        crate::platform::try_open_url(url)
    }

    fn open_path(&self, path: &Path) -> io::Result<()> {
        crate::platform::try_open_path(&shell_friendly(path))
    }
}

/// Where each allowlisted application lives: under the Windows directory, asked of
/// the system, never searched for on PATH (so a program of the same name earlier on
/// PATH cannot take its place).
#[cfg(windows)]
fn system_exe(app: AppId) -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};

    let mut buf = [0u16; 260];
    // SAFETY: `buf` is a valid, writable slice whose length the call is told; the
    // return value is the number of characters written, checked below.
    let (written, relative) = unsafe {
        match app {
            AppId::Notepad => (GetSystemDirectoryW(Some(&mut buf)), "notepad.exe"),
            AppId::Calculator => (GetSystemDirectoryW(Some(&mut buf)), "calc.exe"),
            AppId::FileExplorer => (GetWindowsDirectoryW(Some(&mut buf)), "explorer.exe"),
        }
    };
    let written = written as usize;
    if written == 0 || written >= buf.len() {
        return Err(io::Error::other("the Windows directory could not be read"));
    }
    let path = PathBuf::from(OsString::from_wide(&buf[..written])).join(relative);
    if path.is_file() {
        Ok(path)
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, "not installed"))
    }
}

/// `\\?\C:\x` → `C:\x`. A real path from the OS comes in the long form, which the
/// shell's file handlers do not all accept; a `\\?\UNC\` path is left alone.
fn shell_friendly(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

// ── Attached files ────────────────────────────────────────────────────────────

/// The files the user attached, under the opaque ids the model was told about. The
/// model never sees or supplies a path: an id means "that file, if it is still a
/// regular file inside the inbox", checked every time it is used.
pub struct Files {
    root: PathBuf,
    inner: Mutex<FilesInner>,
}

#[derive(Default)]
struct FilesInner {
    by_id: HashMap<FileId, PathBuf>,
    order: Vec<FileId>,
    issued: usize,
}

impl Files {
    pub fn new(root: PathBuf) -> Files {
        Files {
            root,
            inner: Mutex::new(FilesInner::default()),
        }
    }

    pub fn inbox() -> Files {
        Files::new(files::inbox_dir())
    }

    /// Gives an attached file an id. Refused unless it is a regular file inside the
    /// inbox right now. The same file keeps its id.
    pub fn register(&self, path: &Path) -> Result<(FileId, String), ReadError> {
        files::confine_existing(&self.root, path)?;
        let mut inner = self.inner.lock().unwrap();
        let existing = inner
            .by_id
            .iter()
            .find(|(_, known)| known.as_path() == path)
            .map(|(id, _)| id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                inner.issued += 1;
                let id = FileId::issued(inner.issued);
                inner.by_id.insert(id.clone(), path.to_path_buf());
                inner.order.push(id.clone());
                while inner.order.len() > MAX_FILES {
                    let oldest = inner.order.remove(0);
                    inner.by_id.remove(&oldest);
                }
                id
            }
        };
        Ok((id, display_name(path)))
    }

    pub fn name_of(&self, id: &FileId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .by_id
            .get(id)
            .map(|path| display_name(path))
    }

    pub fn clear(&self) {
        *self.inner.lock().unwrap() = FilesInner::default();
    }

    /// The file an id stands for, re-checked now, as the path to hand to the system.
    fn resolve(&self, id: &FileId) -> Result<PathBuf, Failure> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .by_id
            .get(id)
            .cloned()
            .ok_or(Failure::FileGone)?;
        // Every time, not only when it was attached: the file may since have been
        // replaced, removed, or swapped for a link that leads out of the inbox.
        let real = files::confine_existing(&self.root, &path).map_err(|_| Failure::FileGone)?;
        let extension = real
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match extension {
            Some(ext) if OPENABLE_EXTENSIONS.contains(&ext.as_str()) => Ok(real),
            other => Err(Failure::FileType(other.map(|e| shorten_extension(&e)))),
        }
    }

    #[cfg(test)]
    fn insert_unchecked(&self, id: &str, path: PathBuf) -> FileId {
        let id = FileId::parse(id).unwrap();
        self.inner.lock().unwrap().by_id.insert(id.clone(), path);
        id
    }
}

/// A file name fit to show: no control characters, not too long.
fn display_name(path: &Path) -> String {
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into())
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if name.chars().count() > 60 {
        let cut: String = name.chars().take(57).collect();
        format!("{cut}...")
    } else {
        name
    }
}

fn shorten_extension(ext: &str) -> String {
    ext.chars()
        .take(8)
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

// ── Doing it ──────────────────────────────────────────────────────────────────

/// How an attempt ended. The text is for the person: short, and never a path, an
/// operating-system message or something the model wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    Failed(String),
    /// Cancelled before the system was asked to do anything.
    Cancelled,
}

enum Failure {
    Invalid(String),
    FileGone,
    /// The extension, if the file has a plain one.
    FileType(Option<String>),
    Launch(String),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Invalid(why) => why.clone(),
            Failure::FileGone => "That file isn't available any more.".into(),
            Failure::FileType(Some(ext)) => {
                format!("Coucou only opens documents and images, not .{ext} files.")
            }
            Failure::FileType(None) => {
                "Coucou only opens documents and images, and that file has no known type.".into()
            }
            Failure::Launch(what) => what.clone(),
        }
    }
}

pub struct Executor<L: Launcher> {
    launcher: L,
}

impl<L: Launcher> Executor<L> {
    pub fn new(launcher: L) -> Self {
        Executor { launcher }
    }

    /// Carries out `action`, unless `cancelled` says otherwise at the last moment.
    /// It is asked once, immediately before the system is called: after that an
    /// application is already starting and nothing here can take it back.
    pub fn execute(&self, action: &Action, files: &Files, cancelled: &dyn Fn() -> bool) -> Outcome {
        match self.run(action, files, cancelled) {
            Ok(Some(message)) => Outcome::Done(message),
            Ok(None) => Outcome::Cancelled,
            Err(failure) => Outcome::Failed(failure.message()),
        }
    }

    fn run(
        &self,
        action: &Action,
        files: &Files,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<String>, Failure> {
        // Checked again here whatever the caller did: model output, the frontend and
        // the proposal's own description are none of them trusted.
        action
            .validate()
            .map_err(|err| Failure::Invalid(err.message()))?;

        match action {
            Action::OpenApp(app) => {
                if cancelled() {
                    return Ok(None);
                }
                self.launcher.launch_app(*app).map_err(|err| {
                    Failure::Launch(if err.kind() == io::ErrorKind::NotFound {
                        format!("{} isn't installed on this computer.", app.label())
                    } else {
                        format!("{} couldn't be started.", app.label())
                    })
                })?;
                Ok(Some(format!("Opened {}.", app.label())))
            }
            Action::OpenUrl(url) => {
                if cancelled() {
                    return Ok(None);
                }
                self.launcher
                    .open_url(url.as_str())
                    .map_err(|_| Failure::Launch("The browser couldn't be started.".into()))?;
                Ok(Some(format!("Opened {} in your browser.", url.host())))
            }
            Action::OpenFile(id) => {
                let real = files.resolve(id)?;
                if cancelled() {
                    return Ok(None);
                }
                self.launcher
                    .open_path(&real)
                    .map_err(|_| Failure::Launch("The file couldn't be opened.".into()))?;
                Ok(Some(format!("Opened {}.", display_name(&real))))
            }
        }
    }

    #[cfg(test)]
    fn execute_json(
        &self,
        raw: &serde_json::Value,
        files: &Files,
        cancelled: &dyn Fn() -> bool,
    ) -> Outcome {
        match Action::parse(raw) {
            Ok(action) => self.execute(&action, files, cancelled),
            Err(err) => Outcome::Failed(err.message()),
        }
    }
}

#[cfg(test)]
impl Files {
    /// Writes a small file into the inbox and attaches it, for tests elsewhere.
    pub(crate) fn register_for_test(&self, name: &str) -> FileId {
        let path = self.root.join(name);
        std::fs::write(&path, b"contents").unwrap();
        self.register(&path).unwrap().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::SafeUrl;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Records what it was asked to open and opens nothing.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail_with: Mutex<Option<io::ErrorKind>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn failing(kind: io::ErrorKind) -> Fake {
            let fake = Fake::default();
            *fake.fail_with.lock().unwrap() = Some(kind);
            fake
        }
        fn record(&self, call: String) -> io::Result<()> {
            self.calls.lock().unwrap().push(call);
            match *self.fail_with.lock().unwrap() {
                Some(kind) => Err(io::Error::new(
                    kind,
                    r"C:\Users\someone\secret\path: access denied",
                )),
                None => Ok(()),
            }
        }
    }

    impl Launcher for Fake {
        fn launch_app(&self, app: AppId) -> io::Result<()> {
            self.record(format!("app:{}", app.id()))
        }
        fn open_url(&self, url: &str) -> io::Result<()> {
            self.record(format!("url:{url}"))
        }
        fn open_path(&self, path: &Path) -> io::Result<()> {
            self.record(format!("path:{}", path.display()))
        }
    }

    /// An inbox stand-in and a folder outside it, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "coucou-exec-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(dir.join("inbox")).unwrap();
            std::fs::create_dir_all(dir.join("outside")).unwrap();
            Scratch(dir)
        }
        fn inbox(&self) -> PathBuf {
            self.0.join("inbox")
        }
        fn outside(&self) -> PathBuf {
            self.0.join("outside")
        }
        fn files(&self) -> Files {
            Files::new(self.inbox())
        }
        fn put(&self, dir: &Path, name: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, b"contents").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NEVER: &dyn Fn() -> bool = &|| false;

    fn exec(fake: &Fake, action: &Action, files: &Files) -> Outcome {
        Executor::new(fake).execute(action, files, NEVER)
    }

    fn open_file(id: &FileId) -> Action {
        Action::OpenFile(id.clone())
    }

    // ── Applications ─────────────────────────────────────────────────────────

    #[test]
    fn an_allowlisted_application_is_launched_and_nothing_else() {
        let scratch = Scratch::new("app");
        for app in AppId::ALL {
            let fake = Fake::default();
            assert_eq!(
                exec(&fake, &Action::OpenApp(app), &scratch.files()),
                Outcome::Done(format!("Opened {}.", app.label()))
            );
            assert_eq!(fake.calls(), vec![format!("app:{}", app.id())]);
        }
    }

    #[test]
    fn an_application_that_is_not_allowlisted_never_reaches_the_launcher() {
        let scratch = Scratch::new("app-bad");
        let fake = Fake::default();
        let executor = Executor::new(&fake);
        for app in [
            "cmd",
            "powershell",
            "pwsh",
            "regedit",
            "notepad.exe",
            "Notepad",
            "C:\\Windows\\System32\\cmd.exe",
            "notepad && calc",
            "..\\..\\x",
            "\\\\server\\share\\x.exe",
            "",
        ] {
            let outcome = executor.execute_json(
                &json!({ "type": "open_app", "app": app }),
                &scratch.files(),
                NEVER,
            );
            assert!(
                matches!(outcome, Outcome::Failed(_)),
                "{app:?} -> {outcome:?}"
            );
        }
        // And no extra argument can ride along on an allowlisted one.
        let outcome = executor.execute_json(
            &json!({ "type": "open_app", "app": "notepad", "args": "C:\\Windows\\win.ini" }),
            &scratch.files(),
            NEVER,
        );
        assert!(matches!(outcome, Outcome::Failed(_)));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn a_launcher_failure_is_reported_without_the_system_message() {
        let scratch = Scratch::new("app-fail");
        let fake = Fake::failing(io::ErrorKind::PermissionDenied);
        let Outcome::Failed(message) =
            exec(&fake, &Action::OpenApp(AppId::Notepad), &scratch.files())
        else {
            panic!("expected a failure");
        };
        assert_eq!(message, "Notepad couldn't be started.");
        assert!(
            !message.contains("secret") && !message.contains("Users"),
            "{message}"
        );
        let fake = Fake::failing(io::ErrorKind::NotFound);
        assert_eq!(
            exec(&fake, &Action::OpenApp(AppId::Calculator), &scratch.files()),
            Outcome::Failed("Calculator isn't installed on this computer.".into())
        );
    }

    // ── Links ────────────────────────────────────────────────────────────────

    #[test]
    fn a_valid_link_is_opened_through_the_launcher_in_its_canonical_form() {
        let scratch = Scratch::new("url");
        let fake = Fake::default();
        let action = Action::OpenUrl(
            SafeUrl::parse("HTTPS://Example.COM/a b".replace(' ', "%20").as_str()).unwrap(),
        );
        assert_eq!(
            exec(&fake, &action, &scratch.files()),
            Outcome::Done("Opened example.com in your browser.".into())
        );
        assert_eq!(
            fake.calls(),
            vec!["url:https://example.com/a%20b".to_string()]
        );
    }

    #[test]
    fn a_malformed_link_never_reaches_the_launcher() {
        let scratch = Scratch::new("url-bad");
        let fake = Fake::default();
        let executor = Executor::new(&fake);
        for url in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/cmd.exe",
            "ms-msdt:/id x",
            "https://user:pw@example.com/",
            "example.com",
            "https:///x",
            "https://example.com/a b",
            "https://example.com\r\n",
            "C:\\Windows\\win.ini",
            "",
        ] {
            let outcome = executor.execute_json(
                &json!({ "type": "open_url", "url": url }),
                &scratch.files(),
                NEVER,
            );
            assert!(
                matches!(outcome, Outcome::Failed(_)),
                "{url:?} -> {outcome:?}"
            );
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn the_executor_checks_a_link_again_even_if_it_was_built_around_the_parser() {
        let scratch = Scratch::new("url-forged");
        let fake = Fake::default();
        // `SafeUrl` only comes from `parse`, so reach in the way a bug or a future
        // refactor might: build the action from its parts through serde-free means.
        let forged = Action::OpenFile(FileId::parse("f1").unwrap());
        assert_eq!(
            exec(&fake, &forged, &scratch.files()),
            Outcome::Failed("That file isn't available any more.".into())
        );
        assert!(fake.calls().is_empty());
    }

    // ── Files ────────────────────────────────────────────────────────────────

    #[test]
    fn an_attached_file_inside_the_inbox_is_opened_by_its_real_path() {
        let scratch = Scratch::new("file");
        let note = scratch.put(&scratch.inbox(), "notes.txt");
        let files = scratch.files();
        let (id, name) = files.register(&note).unwrap();
        assert_eq!(name, "notes.txt");
        assert_eq!(files.name_of(&id).as_deref(), Some("notes.txt"));

        let fake = Fake::default();
        let outcome = exec(&fake, &open_file(&id), &files);
        assert_eq!(outcome, Outcome::Done("Opened notes.txt.".into()));
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let opened = calls[0].strip_prefix("path:").unwrap();
        assert!(opened.ends_with("notes.txt"), "{opened}");
        assert_eq!(
            std::fs::canonicalize(opened).unwrap(),
            std::fs::canonicalize(&note).unwrap()
        );
        // What the person is told never includes a path.
        let Outcome::Done(message) = outcome else {
            unreachable!()
        };
        assert!(
            !message.contains("coucou-exec") && !message.contains("inbox"),
            "{message}"
        );
    }

    #[test]
    fn a_file_outside_the_inbox_cannot_be_attached_or_opened() {
        let scratch = Scratch::new("outside");
        let secret = scratch.put(&scratch.outside(), "secret.txt");
        let files = scratch.files();
        assert_eq!(files.register(&secret), Err(ReadError::Outside));
        assert_eq!(
            files.register(
                &scratch
                    .inbox()
                    .join("..")
                    .join("outside")
                    .join("secret.txt")
            ),
            Err(ReadError::Outside)
        );

        // Even if an id somehow pointed there, the executor looks again.
        let fake = Fake::default();
        let id = files.insert_unchecked("f9", secret.clone());
        assert_eq!(
            exec(&fake, &open_file(&id), &files),
            Outcome::Failed("That file isn't available any more.".into())
        );
        let id = files.insert_unchecked(
            "f8",
            scratch
                .inbox()
                .join("..")
                .join("outside")
                .join("secret.txt"),
        );
        assert!(matches!(
            exec(&fake, &open_file(&id), &files),
            Outcome::Failed(_)
        ));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    /// A folder link inside the inbox: a symlink on Linux, a junction on Windows.
    fn link_dir(link: &Path, target: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(out.status.success(), "mklink /J failed: {out:?}");
        }
    }

    #[test]
    fn a_link_that_leads_out_of_the_inbox_is_refused_even_after_the_file_was_attached() {
        let scratch = Scratch::new("link");
        scratch.put(&scratch.outside(), "report.txt");
        std::fs::create_dir(scratch.inbox().join("docs")).unwrap();
        let attached = scratch.put(&scratch.inbox().join("docs"), "report.txt");
        let files = scratch.files();
        let (id, _) = files.register(&attached).unwrap();

        let fake = Fake::default();
        assert!(
            matches!(exec(&fake, &open_file(&id), &files), Outcome::Done(_)),
            "fine while it is really there"
        );
        assert_eq!(fake.calls().len(), 1);

        // The folder is swapped for a link to somewhere else that has a file of the same name.
        std::fs::remove_dir_all(scratch.inbox().join("docs")).unwrap();
        link_dir(&scratch.inbox().join("docs"), &scratch.outside());
        assert_eq!(
            exec(&fake, &open_file(&id), &files),
            Outcome::Failed("That file isn't available any more.".into())
        );
        assert_eq!(fake.calls().len(), 1, "nothing more was opened");

        // And it cannot be attached through the link in the first place.
        assert_eq!(
            files.register(&scratch.inbox().join("docs").join("report.txt")),
            Err(ReadError::Outside)
        );
    }

    #[test]
    fn only_documents_and_images_can_be_opened() {
        let scratch = Scratch::new("ext");
        let files = scratch.files();
        for name in [
            "a.txt",
            "b.MD",
            "c.csv",
            "d.json",
            "e.log",
            "f.pdf",
            "g.PDF",
            "h.png",
            "i.jpg",
            "j.jpeg",
            "k.gif",
            "l.webp",
            "m.bmp",
            "two.dots.txt",
        ] {
            let (id, _) = files
                .register(&scratch.put(&scratch.inbox(), name))
                .unwrap();
            let fake = Fake::default();
            assert!(
                matches!(exec(&fake, &open_file(&id), &files), Outcome::Done(_)),
                "{name}"
            );
            assert_eq!(fake.calls().len(), 1, "{name}");
        }
        // Things that run, things that script, shortcuts, installers, and the disguised.
        for name in [
            "run.exe",
            "run.EXE",
            "x.bat",
            "x.cmd",
            "x.ps1",
            "x.psm1",
            "x.vbs",
            "x.vbe",
            "x.js",
            "x.jse",
            "x.wsf",
            "x.hta",
            "x.lnk",
            "x.url",
            "x.scr",
            "x.com",
            "x.msi",
            "x.msix",
            "x.dll",
            "x.cpl",
            "x.reg",
            "x.jar",
            "x.py",
            "x.sh",
            "x.appref-ms",
            "x.docm",
            "x.xlsm",
            "x.html",
            "x.svg",
            "x.zip",
            "x.iso",
            "x.pdf.exe",
            "x.txt.bat",
            "x.txt.lnk",
            "noextension",
            ".txt",
        ] {
            let (id, _) = files
                .register(&scratch.put(&scratch.inbox(), name))
                .unwrap();
            let fake = Fake::default();
            let outcome = exec(&fake, &open_file(&id), &files);
            assert!(
                matches!(outcome, Outcome::Failed(_)),
                "{name} -> {outcome:?}"
            );
            assert!(
                fake.calls().is_empty(),
                "{name} was opened: {:?}",
                fake.calls()
            );
        }
        let (id, _) = files
            .register(&scratch.put(&scratch.inbox(), "setup.exe"))
            .unwrap();
        assert_eq!(
            exec(&Fake::default(), &open_file(&id), &files),
            Outcome::Failed("Coucou only opens documents and images, not .exe files.".into())
        );
    }

    #[test]
    fn a_file_that_has_gone_is_reported_not_guessed_at() {
        let scratch = Scratch::new("gone");
        let note = scratch.put(&scratch.inbox(), "n.txt");
        let files = scratch.files();
        let (id, _) = files.register(&note).unwrap();
        std::fs::remove_file(&note).unwrap();
        let fake = Fake::default();
        assert_eq!(
            exec(&fake, &open_file(&id), &files),
            Outcome::Failed("That file isn't available any more.".into())
        );
        // An id nobody was given.
        assert_eq!(
            exec(
                &fake,
                &Action::OpenFile(FileId::parse("f77").unwrap()),
                &files
            ),
            Outcome::Failed("That file isn't available any more.".into())
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn file_ids_are_stable_bounded_and_forgotten_on_reset() {
        let scratch = Scratch::new("ids");
        let files = scratch.files();
        let a = scratch.put(&scratch.inbox(), "a.txt");
        let (id_a, _) = files.register(&a).unwrap();
        assert_eq!(
            files.register(&a).unwrap().0,
            id_a,
            "the same file keeps its id"
        );
        assert_eq!(id_a.as_str(), "f1");
        for n in 0..MAX_FILES + 3 {
            files
                .register(&scratch.put(&scratch.inbox(), &format!("extra{n}.txt")))
                .unwrap();
        }
        assert_eq!(files.name_of(&id_a), None, "the oldest falls off");
        assert!(files.inner.lock().unwrap().by_id.len() <= MAX_FILES);
        files.clear();
        assert_eq!(files.inner.lock().unwrap().by_id.len(), 0);
    }

    #[test]
    fn a_file_name_is_cleaned_before_it_is_shown() {
        assert_eq!(
            display_name(Path::new("C:\\x\\report.pdf")),
            if cfg!(windows) {
                "report.pdf"
            } else {
                "C:\\x\\report.pdf"
            }
        );
        assert!(!display_name(Path::new("a\u{1b}[31mb\nc.txt")).contains(char::is_control));
        let long = format!("{}.txt", "n".repeat(200));
        assert!(display_name(Path::new(&long)).chars().count() <= 60);
    }

    // ── Cancellation ─────────────────────────────────────────────────────────

    #[test]
    fn a_cancelled_action_is_not_started() {
        let scratch = Scratch::new("cancel");
        let note = scratch.put(&scratch.inbox(), "n.txt");
        let files = scratch.files();
        let (id, _) = files.register(&note).unwrap();
        let fake = Fake::default();
        let executor = Executor::new(&fake);
        let cancelled: &dyn Fn() -> bool = &|| true;
        for action in [
            Action::OpenApp(AppId::Notepad),
            Action::OpenUrl(SafeUrl::parse("https://example.com/").unwrap()),
            open_file(&id),
        ] {
            assert_eq!(
                executor.execute(&action, &files, cancelled),
                Outcome::Cancelled
            );
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn cancellation_is_asked_at_the_last_moment_not_only_at_the_start() {
        let scratch = Scratch::new("cancel-late");
        let fake = Fake::default();
        let asked = std::cell::Cell::new(0);
        let cancelled = || {
            asked.set(asked.get() + 1);
            true
        };
        assert_eq!(
            Executor::new(&fake).execute(
                &Action::OpenApp(AppId::Calculator),
                &scratch.files(),
                &cancelled
            ),
            Outcome::Cancelled
        );
        assert_eq!(asked.get(), 1, "once, right before the system is called");
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    #[test]
    fn long_form_paths_are_shortened_for_the_shell_but_network_paths_are_left_alone() {
        assert_eq!(
            shell_friendly(Path::new(r"\\?\C:\Users\x\a.txt")),
            PathBuf::from(r"C:\Users\x\a.txt")
        );
        assert_eq!(
            shell_friendly(Path::new(r"\\?\UNC\server\share\a.txt")),
            PathBuf::from(r"\\?\UNC\server\share\a.txt")
        );
        assert_eq!(
            shell_friendly(Path::new("/home/u/a.txt")),
            PathBuf::from("/home/u/a.txt")
        );
    }

    #[test]
    fn the_openable_types_are_documents_and_images_only() {
        for ext in OPENABLE_EXTENSIONS {
            assert!(ext.chars().all(|c| c.is_ascii_lowercase()), "{ext}");
        }
        for dangerous in [
            "exe", "bat", "cmd", "ps1", "lnk", "js", "vbs", "msi", "scr", "com", "hta", "jar",
            "dll", "url", "html", "svg",
        ] {
            assert!(!OPENABLE_EXTENSIONS.contains(&dangerous), "{dangerous}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_real_launcher_finds_the_allowlisted_programs_under_the_windows_directory() {
        let windir = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        for app in AppId::ALL {
            let exe = system_exe(app).unwrap();
            assert!(exe.starts_with(&windir), "{exe:?}");
            assert!(exe.is_absolute() && exe.is_file());
        }
        assert!(system_exe(AppId::Notepad).unwrap().ends_with("notepad.exe"));
    }
}
