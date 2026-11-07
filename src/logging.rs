//! Logging for long-running processes: a size-capped log file, and repeated
//! messages (a computer that stays offline, a failing retry) collapsed so the
//! log stays readable for months of uptime.
use log::{Level, Log, Metadata, Record};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Each log file is rotated at this size; one previous file is kept.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// An identical message is written at most once per window.
const REPEAT_WINDOW: Duration = Duration::from_secs(60);

/// Log to `file` when given, otherwise to standard error.
pub fn init(file: Option<&Path>) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(path) = file
        && let Ok(writer) = Rotating::open(path)
    {
        builder.target(env_logger::Target::Pipe(Box::new(writer)));
    }
    let inner = builder.build();
    let level = inner.filter();
    if log::set_boxed_logger(Box::new(Deduplicating {
        inner,
        last: Mutex::new(None),
    }))
    .is_ok()
    {
        log::set_max_level(level);
    }
    // Background processes have no console: keep panics in the log.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        log::logger().flush();
        default(info);
    }));
}

struct Repeated {
    key: String,
    target: String,
    since: Instant,
    count: u32,
}

struct Deduplicating {
    inner: env_logger::Logger,
    last: Mutex<Option<Repeated>>,
}

/// Messages that differ only in numbers (ports, counts) count as repeats.
fn repeat_key(record: &Record) -> String {
    let mut key = format!("{}|{}|", record.level(), record.target());
    key.extend(
        record
            .args()
            .to_string()
            .chars()
            .filter(|c| !c.is_ascii_digit()),
    );
    key
}

impl Log for Deduplicating {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        let key = repeat_key(record);
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = last.as_mut()
            && previous.key == key
            && previous.since.elapsed() < REPEAT_WINDOW
        {
            previous.count += 1;
            return;
        }
        if let Some(previous) = last.take()
            && previous.count > 0
        {
            self.inner.log(
                &Record::builder()
                    .level(Level::Info)
                    .target(&previous.target)
                    .args(format_args!(
                        "(previous message repeated {} more times)",
                        previous.count
                    ))
                    .build(),
            );
        }
        self.inner.log(record);
        *last = Some(Repeated {
            key,
            target: record.target().to_string(),
            since: Instant::now(),
            count: 0,
        });
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// A log file that moves itself to `<name>.1` once it reaches the size cap.
struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
}

impl Rotating {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let backup = self.path.with_extension("log.1");
        // Windows cannot rename over an existing file or an open one.
        let _ = std::fs::remove_file(&backup);
        let placeholder = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.with_extension("log.tmp"))?;
        drop(std::mem::replace(&mut self.file, placeholder));
        std::fs::rename(&self.path, &backup)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let _ = std::fs::remove_file(self.path.with_extension("log.tmp"));
        self.written = 0;
        Ok(())
    }
}

impl Write for Rotating {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written + bytes.len() as u64 > MAX_LOG_BYTES {
            // A failed rotation must not stop logging; keep appending.
            let _ = self.rotate();
        }
        let written = self.file.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_caps_the_log_and_keeps_one_previous_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent.log");
        let mut log = Rotating::open(&path).unwrap();
        let line = vec![b'x'; 64 * 1024];
        for _ in 0..(MAX_LOG_BYTES / line.len() as u64) * 3 {
            log.write_all(&line).unwrap();
        }
        log.flush().unwrap();
        let current = std::fs::metadata(&path).unwrap().len();
        let previous = std::fs::metadata(path.with_extension("log.1"))
            .unwrap()
            .len();
        assert!(current <= MAX_LOG_BYTES);
        assert!(previous <= MAX_LOG_BYTES);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn messages_differing_only_in_numbers_are_repeats() {
        let first = format_args!("input connection from 192.168.1.2:50001").to_string();
        let record = |text: &str| {
            repeat_key(
                &Record::builder()
                    .args(format_args!("{text}"))
                    .level(Level::Info)
                    .target("t")
                    .build(),
            )
        };
        assert_eq!(
            record(&first),
            record("input connection from 192.168.1.2:50213")
        );
        assert_ne!(record(&first), record("connection refused"));
    }
}
