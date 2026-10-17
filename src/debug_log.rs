use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::Local;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    ENABLED.store(enabled, Ordering::Release);
}

pub fn path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.parent()
                    .map(|parent| parent.join("hidden-screen-recorder-debug.log"))
            })
            .unwrap_or_else(|| PathBuf::from("hidden-screen-recorder-debug.log"))
    })
}

pub fn reset() {
    let _guard = LOG_LOCK.lock().ok();
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let _ = fs::write(path(), "");
}

pub fn write(message: impl AsRef<str>) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let _guard = LOG_LOCK.lock().ok();
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path()) else {
        return;
    };
    let _ = writeln!(
        file,
        "{} [{}] {}",
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        std::thread::current().name().unwrap_or("unnamed"),
        message.as_ref()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_logging_neither_creates_nor_changes_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("test.log");
        LOG_PATH.set(log.clone()).unwrap();

        set_enabled(false);
        reset();
        write("disabled");
        assert!(!log.exists());

        set_enabled(true);
        write("enabled");
        // Synchronize with writes from other parallel tests before taking
        // the snapshot whose immutability we are checking.
        set_enabled(false);
        let contents = fs::read_to_string(&log).unwrap();
        assert!(contents.contains("enabled"));

        set_enabled(false);
        write("disabled again");
        reset();
        assert_eq!(fs::read_to_string(&log).unwrap(), contents);

        fs::remove_file(log).unwrap();
    }
}
