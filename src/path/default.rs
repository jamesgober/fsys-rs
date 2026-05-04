//! Per-OS default [`super::PathSet`] builders.
//!
//! Two flavours: [`build_dev`] returns CWD-relative paths used during
//! development (`./data`, `./logs`, ...) and [`build_prod`] returns the
//! OS-standard system paths described in `.dev/PLANNING.md`.

use std::path::PathBuf;

use super::PathSet;

/// Builds the [`PathSet`] for [`super::Mode::Dev`] — every path is
/// relative to the current working directory.
#[must_use]
pub(super) fn build_dev() -> PathSet {
    PathSet {
        data: PathBuf::from("./data"),
        bin: PathBuf::from("./bin"),
        config: PathBuf::from("./config"),
        logs: PathBuf::from("./logs"),
        cache: PathBuf::from("./cache"),
        libs: PathBuf::from("./libs"),
        runtime: PathBuf::from("./runtime"),
        temp: PathBuf::from("./tmp"),
        state: PathBuf::from("./state"),
        locks: PathBuf::from("./locks"),
    }
}

/// Builds the [`PathSet`] for [`super::Mode::Prod`] using the active
/// platform's conventions.
#[must_use]
pub(super) fn build_prod() -> PathSet {
    #[cfg(target_os = "linux")]
    {
        linux_prod()
    }
    #[cfg(target_os = "macos")]
    {
        macos_prod()
    }
    #[cfg(target_os = "windows")]
    {
        windows_prod()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        // Generic Unix fallback. Conservative; matches the Linux
        // defaults except the lock directory which lives under
        // /var/run on most BSDs.
        PathSet {
            data: PathBuf::from("/var/lib"),
            bin: PathBuf::from("/usr/local/bin"),
            config: PathBuf::from("/etc"),
            logs: PathBuf::from("/var/log"),
            cache: PathBuf::from("/var/cache"),
            libs: PathBuf::from("/usr/local/lib"),
            runtime: PathBuf::from("/var/run"),
            temp: PathBuf::from("/tmp"),
            state: PathBuf::from("/var/lib"),
            locks: PathBuf::from("/var/run"),
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_prod() -> PathSet {
    PathSet {
        data: PathBuf::from("/var/lib"),
        bin: PathBuf::from("/usr/local/bin"),
        config: PathBuf::from("/etc"),
        logs: PathBuf::from("/var/log"),
        cache: PathBuf::from("/var/cache"),
        libs: PathBuf::from("/usr/local/lib"),
        runtime: PathBuf::from("/run"),
        temp: PathBuf::from("/tmp"),
        state: PathBuf::from("/var/lib"),
        locks: PathBuf::from("/run/lock"),
    }
}

#[cfg(target_os = "macos")]
fn macos_prod() -> PathSet {
    let home = home_dir();
    PathSet {
        data: home.join("Library/Application Support"),
        bin: PathBuf::from("/Applications"),
        config: home.join("Library/Preferences"),
        logs: home.join("Library/Logs"),
        cache: home.join("Library/Caches"),
        libs: PathBuf::from("/usr/local/lib"),
        runtime: PathBuf::from("/var/run"),
        temp: PathBuf::from("/tmp"),
        state: home.join("Library/Application Support"),
        locks: PathBuf::from("/var/run"),
    }
}

#[cfg(target_os = "windows")]
fn windows_prod() -> PathSet {
    let program_data = env_or("ProgramData", "C:\\ProgramData");
    let program_files = env_or("ProgramFiles", "C:\\Program Files");
    let local_appdata = env_or("LOCALAPPDATA", "C:\\Users\\Default\\AppData\\Local");
    let temp = env_or("TEMP", "C:\\Windows\\Temp");

    PathSet {
        data: program_data.clone(),
        bin: program_files.clone(),
        config: program_data.clone(),
        logs: program_data.join("Logs"),
        cache: local_appdata.clone(),
        libs: program_files,
        runtime: temp.clone(),
        temp: temp.clone(),
        state: local_appdata,
        locks: temp,
    }
}

#[cfg(any(
    target_os = "macos",
    all(unix, not(any(target_os = "linux", target_os = "windows")))
))]
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "windows")]
fn env_or(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dev_paths_are_all_relative() {
        let set = build_dev();
        for p in [
            &set.data,
            &set.bin,
            &set.config,
            &set.logs,
            &set.cache,
            &set.libs,
            &set.runtime,
            &set.temp,
            &set.state,
            &set.locks,
        ] {
            assert!(p.is_relative(), "expected relative path, got {:?}", p);
        }
    }

    #[test]
    fn test_dev_data_is_dot_data() {
        assert_eq!(build_dev().data, PathBuf::from("./data"));
    }

    #[test]
    fn test_prod_paths_are_non_empty() {
        let set = build_prod();
        for p in [
            &set.data,
            &set.bin,
            &set.config,
            &set.logs,
            &set.cache,
            &set.libs,
            &set.runtime,
            &set.temp,
            &set.state,
            &set.locks,
        ] {
            assert!(!p.as_os_str().is_empty());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_prod_data_is_var_lib() {
        assert_eq!(build_prod().data, PathBuf::from("/var/lib"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_prod_logs_is_var_log() {
        assert_eq!(build_prod().logs, PathBuf::from("/var/log"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_windows_prod_paths_resolve_env_or_fallback() {
        let set = build_prod();
        // We can't assert exact paths because they depend on the
        // host's env, but they must be absolute when env is present.
        if std::env::var_os("ProgramData").is_some() {
            assert!(set.data.is_absolute() || set.data.has_root());
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_prod_data_under_library() {
        let data = build_prod().data;
        let s = data.to_string_lossy();
        assert!(s.contains("Library/Application Support"));
    }
}
