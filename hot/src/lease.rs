use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
pub struct ExecutionLease {
    _file: File,
    pub path: PathBuf,
}
impl ExecutionLease {
    pub fn acquire_for_funder(funder: &str) -> Result<Self, String> {
        let wallet: String = funder
            .trim_start_matches("0x")
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if wallet.len() != 40 {
            return Err(format!("cannot derive execution lease from funder {funder:?}"));
        }
        Self::acquire_path(PathBuf::from(format!("/tmp/copybot-{wallet}.lease")))
    }
    fn acquire_path(path: PathBuf) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("open execution lease {}: {e}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(
                format!(
                    "execution lease {} is already held; another live process is using this wallet",
                    path.display()
                ),
            );
        }
        file.set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|_| writeln!(file, "pid={}", std::process::id()))
            .and_then(|_| file.sync_data())
            .map_err(|e| format!("write execution lease {}: {e}", path.display()))?;
        Ok(Self { _file: file, path })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(
                format!(
                    "copybot-lease-{name}-{}-{}", std::process::id(), crate
                    ::ledger::now_secs()
                ),
            )
    }
    #[test]
    fn a_second_process_handle_cannot_acquire_the_same_wallet_lease() {
        let p = temp_path("exclusive");
        let first = ExecutionLease::acquire_path(p.clone()).unwrap();
        let second = ExecutionLease::acquire_path(p.clone());
        assert!(second.is_err(), "a duplicate execution process must fail immediately");
        drop(first);
        let third = ExecutionLease::acquire_path(p.clone());
        assert!(third.is_ok(), "a crashed/stopped process must release the OS lock");
        drop(third);
        let _ = std::fs::remove_file(p);
    }
    #[test]
    fn funder_identity_must_be_a_complete_address() {
        assert!(ExecutionLease::acquire_for_funder("0x1234").is_err());
    }
}
