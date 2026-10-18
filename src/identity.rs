use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub fn random_key() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("random key generation failed"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn actor(key: &str) -> String {
    digest::digest(&digest::SHA256, key.to_ascii_lowercase().as_bytes()).as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn load_key(path: &Path) -> io::Result<String> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            let key = random_key()?;
            writeln!(file, "{key}")?;
            file.sync_all()?;
            Ok(key)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let mut key = String::new();
            File::open(path)?.take(1024).read_to_string(&mut key)?;
            let key = key.trim().to_owned();
            if !valid_key(&key) {
                return Err(io::Error::other(
                    "identity file must contain a 64-digit hexadecimal key",
                ));
            }
            Ok(key)
        }
        Err(e) => Err(e),
    }
}
/// Server and administrator CLI take the same OS lock. It is released on exit,
/// including crashes; no stale PID-file recovery or unsafe concurrent maintenance.
pub fn lock_database(path: &Path) -> io::Result<File> {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock)?;
    // SAFETY: file owns a valid descriptor for the entire lock lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other(
            "database is in use; stop hearth-ircd before administrator maintenance",
        ));
    }
    Ok(file)
}
