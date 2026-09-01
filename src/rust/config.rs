use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use uuid::Uuid;

const WINDOWS_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub output_directory: Option<PathBuf>,
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    fs::write(&temporary, bytes)?;

    if let Err(error) = retry_transient_windows_file_operation(|| replace_file(&temporary, path)) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn retry_transient_windows_file_operation<T>(
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut delays = WINDOWS_FILE_RETRY_DELAYS.into_iter();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_transient_windows_file_error(&error) => {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                thread::sleep(delay);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_transient_windows_file_error(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if !destination.exists() {
        return fs::rename(temporary, destination);
    }

    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();

    let replaced = unsafe {
        ReplaceFileW(
            destination_wide.as_ptr(),
            temporary_wide.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_json_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let expected = AppConfig {
            output_directory: Some(PathBuf::from("C:/Videos")),
        };
        write_json_atomic(&path, &expected).unwrap();
        let actual: AppConfig = read_json(&path).unwrap().unwrap();
        assert_eq!(actual.output_directory, expected.output_directory);

        let replacement = AppConfig {
            output_directory: Some(PathBuf::from("D:/Media")),
        };
        write_json_atomic(&path, &replacement).unwrap();
        let actual: AppConfig = read_json(&path).unwrap().unwrap();
        assert_eq!(actual.output_directory, replacement.output_directory);
    }
}
