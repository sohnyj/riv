//! Remote image fetch delegated to the Windows in-box curl.exe (System32).

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// Download ceiling, matching the archive member cap.
const MAXIMUM_DOWNLOAD_BYTES: u64 = crate::archive::reader::MAXIMUM_MEMBER_BYTES;

/// Receive chunk; cancellation is checked between chunks.
const READ_BLOCK_BYTES: usize = 256 * 1024;

const SUPPORTED_PROTOCOLS: &[&str] = &["http", "https"];

pub struct NetworkError {
    pub message: String,
    pub code: i32,
    pub cancelled: bool,
}

impl NetworkError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 0,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: "cancelled".to_string(),
            code: 0,
            cancelled: true,
        }
    }
}

/// System32\curl.exe, in-box since Windows 10 1803; the startup version check guarantees it.
fn executable_path() -> PathBuf {
    let mut buffer = [0u16; MAX_PATH as usize];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    crate::text::path_from_wide(&buffer[..length]).join("curl.exe")
}

pub fn is_supported_protocol(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        SUPPORTED_PROTOCOLS
            .iter()
            .any(|protocol| scheme.eq_ignore_ascii_case(protocol))
    })
}

/// Last URL path segment for titles; the whole URL when there is none.
pub fn file_name(url: &str) -> &str {
    path_segment(url)
        .filter(|segment| !segment.is_empty())
        .unwrap_or(url)
}

/// Extension of the URL path's last segment; query and fragment do not count.
pub fn extension_lowercase(url: &str) -> Option<String> {
    let (stem, extension) = path_segment(url)?.rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() || !extension.chars().all(char::is_alphanumeric) {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

fn path_segment(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = after_scheme
        .split_once(['?', '#'])
        .map_or(after_scheme, |(before, _)| before);
    let (_, segments) = path.split_once('/')?;
    Some(segments.rsplit_once('/').map_or(segments, |(_, name)| name))
}

/// Fetches a URL to memory; the protocol check doubles as argument-injection defense.
pub fn download(
    url: &str,
    cancellation: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<Vec<u8>, NetworkError> {
    if !is_supported_protocol(url) {
        return Err(NetworkError::new("Unsupported URL protocol"));
    }
    let maximum_bytes = MAXIMUM_DOWNLOAD_BYTES.to_string();
    let protocol_allowlist = format!("={}", SUPPORTED_PROTOCOLS.join(","));
    let mut child = Command::new(executable_path())
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-redirs",
            "10",
            "--globoff",
            "--proto",
            protocol_allowlist.as_str(),
            "--proto-redir",
            protocol_allowlist.as_str(),
            "--max-filesize",
            maximum_bytes.as_str(),
            "--connect-timeout",
            "5",
            // A speed floor drops a stall without a total-time cap on slow-but-real fetches.
            "--speed-limit",
            "1024",
            "--speed-time",
            "10",
            "--output",
            "-",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|error| NetworkError::new(format!("curl couldn't be started: {error}")))?;
    let mut stdout = child.stdout.take().expect("stdout piped above");
    progress(0);
    let mut body = Vec::new();
    let mut block = vec![0u8; READ_BLOCK_BYTES];
    loop {
        if cancellation.load(Ordering::Relaxed) {
            return Err(kill_child(child, NetworkError::cancelled()));
        }
        let read_bytes = match stdout.read(&mut block) {
            Ok(0) => break,
            Ok(read_bytes) => read_bytes,
            Err(error) => {
                return Err(kill_child(
                    child,
                    NetworkError::new(format!("Download read failed: {error}")),
                ));
            }
        };
        if body.len() as u64 + read_bytes as u64 > MAXIMUM_DOWNLOAD_BYTES {
            return Err(kill_child(
                child,
                NetworkError::new(format!(
                    "Download exceeds the {} GiB limit",
                    MAXIMUM_DOWNLOAD_BYTES >> 30
                )),
            ));
        }
        let chunk = &block[..read_bytes];
        if body.try_reserve(chunk.len()).is_err() {
            return Err(kill_child(
                child,
                NetworkError::new("Download is too large to fit in memory"),
            ));
        }
        body.extend_from_slice(chunk);
        progress(body.len() as u64);
    }
    drop(stdout);
    let mut stderr_text = String::new();
    let mut stderr = child.stderr.take().expect("stderr piped above");
    let _ = stderr.read_to_string(&mut stderr_text);
    let status = child
        .wait()
        .map_err(|error| NetworkError::new(format!("curl didn't exit cleanly: {error}")))?;
    if !status.success() {
        let message = stderr_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Download failed")
            .to_string();
        return Err(NetworkError {
            message,
            code: status
                .code()
                .expect("exit codes are always present on Windows"),
            cancelled: false,
        });
    }
    Ok(body)
}

fn kill_child(mut child: Child, error: NetworkError) -> NetworkError {
    let _ = child.kill();
    let _ = child.wait();
    error
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn download_rejects_unsupported_protocols_before_spawning() {
        let cancellation = AtomicBool::new(false);
        let result = download("ftp://127.0.0.1/a.png", &cancellation, &mut |_| {});
        assert!(matches!(result, Err(error) if error.message.contains("protocol")));
    }

    #[test]
    #[ignore = "needs System32 curl.exe"]
    fn downloads_from_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local address").port();
        let body = b"\x89PNG local fixture";
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = [0u8; 2048];
                let length = std::io::Read::read(&mut stream, &mut request).unwrap_or(0);
                let target_found = request[..length]
                    .windows("GET /test.png".len())
                    .any(|window| window == b"GET /test.png");
                let response = if target_found {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    [header.as_bytes(), body].concat()
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                stream.write_all(&response).expect("respond");
            }
        });
        let cancellation = AtomicBool::new(false);
        let mut reports = Vec::new();
        let downloaded_bytes = download(
            &format!("http://127.0.0.1:{port}/test.png"),
            &cancellation,
            &mut |received_bytes| reports.push(received_bytes),
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(downloaded_bytes, body);
        assert_eq!(reports.first(), Some(&0)); // spawn reads as connecting
        assert_eq!(reports.last(), Some(&(body.len() as u64)));
        let missing = download(
            &format!("http://127.0.0.1:{port}/missing.png"),
            &cancellation,
            &mut |_| {},
        );
        // --fail turns the 404 into curl exit code 22 with a stderr message.
        assert!(matches!(missing, Err(error) if error.code == 22 && !error.message.is_empty()));
        server.join().expect("server thread");
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_accepted() {
        assert!(is_supported_protocol("http://a/b.png"));
        assert!(is_supported_protocol("HTTPS://a/b.png"));
        assert!(!is_supported_protocol("ftp://a/b.png"));
        assert!(!is_supported_protocol("file:///c:/b.png"));
        assert!(!is_supported_protocol("C:\\a\\b.png"));
        assert!(!is_supported_protocol("\\\\server\\share\\b.png"));
        assert!(!is_supported_protocol("see https://a/b.png look"));
        assert!(!is_supported_protocol("seehttps://a/b.pnglook"));
        assert!(!is_supported_protocol("no scheme here"));
        assert!(!is_supported_protocol(""));
    }

    #[test]
    fn extensions_come_from_the_path_segment_only() {
        let extension = |url: &str| extension_lowercase(url);
        assert_eq!(extension("https://a.com/b/c.PNG").as_deref(), Some("png"));
        assert_eq!(
            extension("https://a.com/c.jpg?width=1#top").as_deref(),
            Some("jpg")
        );
        assert_eq!(extension("https://a.com/image?name=c.jpg"), None);
        assert_eq!(extension("https://a.com/download"), None);
        assert_eq!(extension("https://a.com"), None); // the host is not a segment
        assert_eq!(extension("https://a.com/.hidden"), None);
        assert_eq!(extension("https://a.com/c."), None);
    }
}
