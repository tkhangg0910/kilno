//! Linux (and other Unix) counterparts of [`crate::win`]'s modules, under the same names, so a
//! call site reads the same on both: `os::shell::open`, `os::http::get_with`, `os::instance`.
//! No `unsafe` here.

pub mod shell {
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn spawn(program: &str, args: &[&str]) -> bool {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }

    /// A URL, file or folder in what the desktop opens it with (`xdg-open`); not waited for.
    pub fn open(target: &str) {
        let _ = spawn("xdg-open", &[target]);
    }

    /// An agent's own file in whatever opens it; failing that, its folder.
    pub fn open_file_or_select(path: &Path) {
        if !spawn("xdg-open", &[&path.to_string_lossy()]) {
            reveal(path);
        }
    }

    /// A folder opens in the file manager; a file is shown selected in its folder where the file
    /// manager supports it (FileManager1 over D-Bus), else its folder opens. Nothing is run.
    fn reveal(path: &Path) {
        if path.is_dir() {
            open(&path.to_string_lossy());
            return;
        }
        let uri = file_uri(path);
        let shown = Command::new("dbus-send")
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !shown {
            if let Some(dir) = path.parent() {
                open(&dir.to_string_lossy());
            }
        }
    }

    /// `file://` and the path, every byte outside the unreserved set and `/` percent-encoded (a
    /// comma would otherwise split dbus-send's array).
    pub fn file_uri(path: &Path) -> String {
        let mut out = String::from("file://");
        for b in path.to_string_lossy().bytes() {
            if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn file_uris_escape_what_could_split_them() {
            let p = std::path::Path::new("/home/me/a b,c%.rs");
            assert_eq!(super::file_uri(p), "file:///home/me/a%20b%2Cc%25.rs");
        }
    }
}

pub mod http {
    use std::io;
    use std::time::Duration;

    /// GET an https URL: (status code, body). Blocks; call it off the UI thread.
    pub fn get(url: &str, user_agent: &str, timeout: Duration) -> io::Result<(u32, Vec<u8>)> {
        get_with(url, user_agent, "", timeout)
    }

    /// As [`get`], with extra request headers (CRLF-separated). A request with headers does not
    /// follow redirects, so what they carry (a sign-in token) reaches the named host only.
    pub fn get_with(url: &str, user_agent: &str, headers: &str, timeout: Duration) -> io::Result<(u32, Vec<u8>)> {
        if !url.starts_with("https://") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not https"));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(if headers.is_empty() { 5 } else { 0 })
            .http_status_as_error(false)
            .user_agent(user_agent)
            .build()
            .into();
        let mut req = agent.get(url);
        for line in headers.split("\r\n").filter(|l| !l.is_empty()) {
            if let Some((k, v)) = line.split_once(':') {
                req = req.header(k.trim(), v.trim());
            }
        }
        let mut resp = req.call().map_err(io::Error::other)?;
        let status = u32::from(resp.status().as_u16());
        // GitHub's and Anthropic's answers are a few KB; anything huge is not what we asked for.
        let body = resp.body_mut().with_config().limit(4 * 1024 * 1024).read_to_vec().map_err(io::Error::other)?;
        Ok((status, body))
    }
}

pub mod instance {
    //! One Kilno per user: the first one listens on a Unix socket in the runtime folder; a second
    //! launch hands it its folder over that socket and exits.

    use std::io::{self, BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    /// How long a second launch waits to reach the first.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    /// A handoff larger than this is not a folder path; the rest is dropped.
    const MAX: usize = 64 * 1024;

    static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

    /// `$XDG_RUNTIME_DIR/kilno.sock` (private to the user), else one in Kilno's data folder.
    pub fn socket_path() -> PathBuf {
        let name = if cfg!(debug_assertions) { "kilno-dev.sock" } else { "kilno.sock" };
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d).join(name),
            None => kilno_core::paths::data_dir(&kilno_core::paths::OsEnv).join(name),
        }
    }

    /// True for the first Kilno: it now owns the socket. False when another one answers on it.
    /// A socket file nobody answers on is left over from a crash, and is taken over.
    pub fn acquire() -> bool {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            return false;
        }
        let _ = std::fs::remove_file(&path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match UnixListener::bind(&path) {
            Ok(l) => {
                if let Ok(mut slot) = LISTENER.lock() {
                    *slot = Some(l);
                }
                true
            }
            // Someone bound it between the two calls: they are the first.
            Err(_) => UnixStream::connect(&path).is_err(),
        }
    }

    /// The first Kilno: each handoff (one line) goes to `on_message`, on a background thread.
    pub fn serve(on_message: impl Fn(String) + Send + 'static) -> io::Result<()> {
        let listener = LISTENER.lock().ok().and_then(|mut s| s.take()).ok_or_else(|| io::Error::other("not acquired"))?;
        std::thread::Builder::new().name("kilno-handoff".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));
                let mut line = String::new();
                if BufReader::new(stream.take_limit()).read_line(&mut line).is_ok() {
                    on_message(line.trim_end_matches(['\r', '\n']).to_string());
                }
            }
        })?;
        Ok(())
    }

    /// A second launch: hand `text` to the first and return.
    pub fn send(text: &str) -> io::Result<()> {
        let mut s = UnixStream::connect(socket_path())?;
        s.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        s.write_all(text.replace(['\r', '\n'], " ").as_bytes())?;
        s.write_all(b"\n")
    }

    trait TakeLimit {
        fn take_limit(self) -> std::io::Take<UnixStream>;
    }
    impl TakeLimit for UnixStream {
        fn take_limit(self) -> std::io::Take<UnixStream> {
            std::io::Read::take(self, MAX as u64)
        }
    }
}

/// What the clipboard holds that a paste can use (as [`crate::win`]'s).
pub enum Clip {
    Text(String),
    Files(Vec<String>),
    Nothing,
}

/// The clipboard through arboard (X11, or Wayland's data-control). Files copied in a file
/// manager arrive as `file://` URIs in the text (one per line), and are pasted as paths.
pub fn read_clipboard() -> Clip {
    let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) else {
        return Clip::Nothing;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if !lines.is_empty() && lines.iter().all(|l| l.starts_with("file://")) {
        let files: Vec<String> = lines.iter().filter_map(|l| uri_path(l.trim())).collect();
        if !files.is_empty() {
            return Clip::Files(files);
        }
    }
    if text.is_empty() {
        Clip::Nothing
    } else {
        Clip::Text(text)
    }
}

/// `file:///a%20b` → `/a b`; None for a URI with a host or bad escapes.
fn uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = rest.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod clip_tests {
    #[test]
    fn file_uris_become_paths() {
        assert_eq!(super::uri_path("file:///home/me/a%20b.png").as_deref(), Some("/home/me/a b.png"));
        assert_eq!(super::uri_path("file://host/x"), None, "no remote hosts");
        assert_eq!(super::uri_path("file:///bad%zz"), None);
    }
}

pub mod tray {
    //! The tray icon off Windows: a StatusNotifierItem (ksni). A click shows the window; its menu
    //! has Show Kilno and Quit Kilno, as on Windows. Where the desktop has no tray (GNOME without
    //! the AppIndicator extension) nothing shows and nothing fails: running `kilno` again brings
    //! the window back.

    use ksni::blocking::TrayMethods;
    use std::sync::{Arc, Mutex};

    pub enum TrayCmd {
        Show,
        Quit,
    }

    type OnCmd = Arc<dyn Fn(TrayCmd) + Send + Sync>;

    struct KilnoTray {
        on_cmd: OnCmd,
    }

    impl ksni::Tray for KilnoTray {
        fn id(&self) -> String {
            "kilno".into()
        }

        fn title(&self) -> String {
            "Kilno".into()
        }

        /// The installed icon (install.sh puts it in the hicolor theme); the pixmap below when
        /// the theme lacks it.
        fn icon_name(&self) -> String {
            "kilno".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            logo().into_iter().collect()
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on_cmd)(TrayCmd::Show);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: "Show Kilno".into(),
                    activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::Show)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Quit Kilno".into(),
                    activate: Box::new(|t: &mut Self| (t.on_cmd)(TrayCmd::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// The logo as the ARGB32 (network byte order) pixmap the protocol carries.
    fn logo() -> Option<ksni::Icon> {
        let img = image::load_from_memory_with_format(include_bytes!("../../../docs/assets/logo.png"), image::ImageFormat::Png)
            .ok()?
            .to_rgba8();
        let (w, h) = img.dimensions();
        let data = img.pixels().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
        Some(ksni::Icon { width: w as i32, height: h as i32, data })
    }

    static HANDLE: Mutex<Option<ksni::blocking::Handle<KilnoTray>>> = Mutex::new(None);

    /// Shows the icon; `on_cmd` runs on the tray's thread.
    pub fn start(on_cmd: impl Fn(TrayCmd) + Send + Sync + 'static) {
        let tray = KilnoTray { on_cmd: Arc::new(on_cmd) };
        if let Ok(handle) = tray.spawn() {
            if let Ok(mut h) = HANDLE.lock() {
                *h = Some(handle);
            }
        }
    }

    /// Takes the icon away (quit).
    pub fn remove() {
        if let Some(h) = HANDLE.lock().ok().and_then(|mut h| h.take()) {
            h.shutdown();
        }
    }
}
