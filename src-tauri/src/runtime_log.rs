//! Bounded persistent diagnostics for Finder-launched overnight sessions.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct RuntimeLog(Arc<Mutex<Option<LogFile>>>);

struct LogFile {
    path: PathBuf,
    file: File,
    size: u64,
    limit: u64,
}

fn append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl RuntimeLog {
    pub fn open(directory: &Path, limit: u64) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("runtime.log");
        let file = append(&path)?;
        let size = file.metadata()?.len();
        Ok(Self(Arc::new(Mutex::new(Some(LogFile {
            path,
            file,
            size,
            limit,
        })))))
    }

    pub fn stderr() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}

impl LogFile {
    fn write_record(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.limit {
            self.file.flush()?;
            let previous = self.path.with_extension("log.1");
            let older = self.path.with_extension("log.2");
            if previous.exists() {
                fs::rename(&previous, &older)?;
            }
            fs::rename(&self.path, previous)?;
            self.file = append(&self.path)?;
            self.size = 0;
        }
        self.file.write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

impl Write for RuntimeLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(file) = state.as_mut() {
            if file.write_record(bytes).is_ok() {
                return Ok(bytes.len());
            }
            // Disk/rotation failure must not break detection or panic.
            *state = None;
        }
        io::stderr().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.as_mut() {
            Some(file) => file.file.flush(),
            None => io::stderr().flush(),
        }
    }
}

#[derive(serde::Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

/// Read only a fixed tail of the known runtime file, never arbitrary user paths.
pub fn read_tail(path: &Path) -> io::Result<Vec<LogEntry>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(256 * 1024).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // Both the leading fragment and a concurrently incomplete final line are omitted.
    let start = if offset > 0 {
        text.find('\n').map(|i| i + 1).unwrap_or(text.len())
    } else {
        0
    };
    let end = text.rfind('\n').unwrap_or(0);
    if start >= end {
        return Ok(Vec::new());
    }
    Ok(text[start..end]
        .lines()
        .rev()
        .filter_map(parse_display_line)
        .take(1000)
        .collect())
}

fn parse_display_line(line: &str) -> Option<LogEntry> {
    let mut fields = line.splitn(2, char::is_whitespace);
    let timestamp = fields.next()?;
    if chrono::DateTime::parse_from_rfc3339(timestamp).is_err() {
        return None;
    }
    let rest = fields.next()?.trim_start();
    let (level, message) = rest.split_once(char::is_whitespace)?;
    if !matches!(level, "INFO" | "WARN" | "ERROR") {
        return None;
    }
    let message = message.trim_start();
    let lower = message.to_ascii_lowercase();
    let words: Vec<_> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|word| !word.is_empty()).collect();
    let sensitive = [
        "authorization",
        "cookie",
        "api_key",
        "apikey",
        "password",
        "secret",
        "sessionid",
        "username",
        "credential",
        "sk-",
        "ark-",
        "bearer",
    ]
    .iter()
    .any(|word| lower.contains(word))
        // Underscores separate credential field components (access_token),
        // but plural usage counters (tokens / prompt_tokens) are safe.
        || words.contains(&"token")
        || words.windows(2)
            .any(|words| words == ["api", "key"]);
    let message = if sensitive {
        "[包含敏感字段，内容已隐藏]".into()
    } else {
        message
            .chars()
            .filter(|c| !c.is_control())
            .take(2000)
            .collect()
    };
    Some(LogEntry {
        timestamp: timestamp.into(),
        level: level.into(),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_logs_hide_sensitive_lines_and_reject_raw_frames() {
        let entry = parse_display_line("2026-09-12T03:00:00Z  WARN auth token=private").unwrap();
        assert!(!entry.message.contains("private"));
        assert!(parse_display_line("raw provider payload").is_none());
        assert!(parse_display_line("2026-09-12T03:00:00Z DEBUG raw payload").is_none());
        assert_eq!(
            parse_display_line("2026-09-12T03:00:00Z INFO connected")
                .unwrap()
                .message,
            "connected"
        );
    }

    #[test]
    fn hides_credential_formats_but_preserves_token_usage() {
        for message in [
            "ark-example-key", "ARK-example-key", "Bearer opaque-value",
            "api key=private", "API   KEY: private", "api\tkey=private",
            "token=private", "access_token=private", "refresh-token: private",
            "token=private prompt_tokens=123", "sessionid=private",
        ] {
            let entry = parse_display_line(&format!("2026-09-12T03:00:00Z WARN {message}")).unwrap();
            assert_eq!(entry.message, "[包含敏感字段，内容已隐藏]", "{message}");
        }
        for message in [
            "prompt_tokens=123 completion_tokens=456 total_tokens=579",
            "total image and text tokens exceed max message tokens",
            "tokenizer ready", "connected", "reasoning_effort=low",
        ] {
            assert_eq!(parse_display_line(&format!("2026-09-12T03:00:00Z INFO {message}")).unwrap().message, message);
        }
    }

    #[test]
    fn tail_is_bounded_newest_first_and_ignores_partial_lines() {
        let path = std::env::temp_dir().join(format!("ict-tail-{}.log", std::process::id()));
        let mut data = "x".repeat(300_000);
        data.push('\n');
        for i in 0..1200 {
            data.push_str(&format!("2026-09-12T03:00:00Z INFO event={i}\n"));
        }
        data.push_str("2026-09-12T03:00:00Z ERROR incomplete");
        fs::write(&path, data).unwrap();
        let entries = read_tail(&path).unwrap();
        assert_eq!(entries.len(), 1000);
        assert_eq!(entries[0].message, "event=1199");
        assert_eq!(entries[999].message, "event=200");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn logs_survive_reopen_and_rotate_with_two_backups() {
        let directory = std::env::temp_dir().join(format!(
            "ict-log-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut log = RuntimeLog::open(&directory, 8).unwrap();
        log.write_all(b"first\n").unwrap();
        drop(log);
        let mut log = RuntimeLog::open(&directory, 8).unwrap();
        log.write_all(b"second\n").unwrap();
        log.write_all(b"third\n").unwrap();
        log.write_all(b"fourth\n").unwrap();
        assert_eq!(
            fs::read(directory.join("runtime.log")).unwrap(),
            b"fourth\n"
        );
        assert_eq!(
            fs::read(directory.join("runtime.log.1")).unwrap(),
            b"third\n"
        );
        assert_eq!(
            fs::read(directory.join("runtime.log.2")).unwrap(),
            b"second\n"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);
        drop(log);
        fs::remove_dir_all(directory).unwrap();
    }
}
