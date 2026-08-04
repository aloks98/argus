// Self-update staging: receive the new binary as UpdateChunk frames, verify,
// swap, and re-exec. Everything here is synchronous std::fs on purpose --
// chunks are 256 KiB writes to local disk, far below the threshold where
// blocking the inbound loop matters, and it keeps the module dependency-free.
//
// Every failure path leaves the CURRENT binary untouched and running; the
// worst possible outcome of a refused update is a deleted temp file.
//
// The final swap prefers `renameat2(RENAME_EXCHANGE)`: both the temp file
// and the exe path change names in a single syscall, so a SIGKILL landing
// mid-swap can never leave the exe path pointing at nothing (a supervisor
// restart hitting ENOENT). The classic two-rename sequence (current -> .old,
// temp -> current) has a window between those two renames where the exe path
// doesn't exist; RENAME_EXCHANGE has no such window. Not every kernel/
// filesystem supports it (older kernels, some non-Linux-native filesystems),
// so an EINVAL/ENOSYS/ENOTSUP falls back to the two-rename sequence, keeping
// its existing best-effort restore.
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Spec cap: anything larger than this is implausible for the agent binary.
const MAX_UPDATE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct Staged {
    pub version: String,
    pub command_id: String,
    pub exe: PathBuf,
}

struct Pending {
    version: String,
    sha256_hex: String,
    total_bytes: u64,
    command_id: String,
    file: File,
    temp: PathBuf,
    received: u64,
    hasher: ring::digest::Context,
}

pub struct Updater {
    exe: PathBuf,
    pending: Option<Pending>,
}

impl Updater {
    /// `exe` = the current binary's path, resolved ONCE (via /proc/self/exe)
    /// before any update can rename things out from under the symlink.
    pub fn new(exe: PathBuf) -> Self {
        Updater { exe, pending: None }
    }

    pub fn begin(
        &mut self,
        version: &str,
        sha256_hex: &str,
        total_bytes: u64,
        command_id: &str,
    ) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("an update is already in flight".into());
        }
        if total_bytes == 0 || total_bytes > MAX_UPDATE_BYTES {
            return Err(format!("implausible total_bytes {total_bytes}"));
        }
        // Same directory as the binary: the final rename must be atomic,
        // which requires the same filesystem.
        let temp = self.temp_path();
        let file = File::create(&temp).map_err(|e| format!("create {}: {e}", temp.display()))?;
        self.pending = Some(Pending {
            version: version.to_string(),
            sha256_hex: sha256_hex.to_lowercase(),
            total_bytes,
            command_id: command_id.to_string(),
            file,
            temp,
            received: 0,
            hasher: ring::digest::Context::new(&ring::digest::SHA256),
        });
        Ok(())
    }

    pub fn chunk(&mut self, data: &[u8], last: bool) -> Result<Option<Staged>, String> {
        let Some(p) = self.pending.as_mut() else {
            return Err("chunk without an announced update".into());
        };
        p.received += data.len() as u64;
        if p.received > p.total_bytes {
            let msg = format!("size overrun: got {} of {}", p.received, p.total_bytes);
            self.abort();
            return Err(msg);
        }
        if let Err(e) = p.file.write_all(data) {
            let msg = format!("write: {e}");
            self.abort();
            return Err(msg);
        }
        p.hasher.update(data);
        if !last {
            return Ok(None);
        }
        if p.received != p.total_bytes {
            let msg = format!("size underrun: got {} of {}", p.received, p.total_bytes);
            self.abort();
            return Err(msg);
        }
        let digest = p.hasher.clone().finish();
        let got: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if got != p.sha256_hex {
            let msg = format!("sha256 mismatch: got {got}, announced {}", p.sha256_hex);
            self.abort();
            return Err(msg);
        }
        if let Err(e) = p.file.sync_all() {
            let msg = format!("fsync: {e}");
            self.abort();
            return Err(msg);
        }
        // Verified. Swap: chmod temp, then exchange it with the current exe
        // in one syscall (never a window where the exe path is empty),
        // falling back to the two-rename sequence when exchange isn't
        // supported.
        let p = self.pending.take().expect("checked above");
        let old = self.old_path();
        let swap = (|| -> std::io::Result<()> {
            fs::set_permissions(&p.temp, fs::Permissions::from_mode(0o755))?;
            match exchange(&p.temp, &self.exe) {
                Ok(()) => {
                    // The swap is already complete and self.exe was never
                    // briefly absent: the OLD binary now sits at the temp
                    // path (names exchanged), so move it to .old for the
                    // archival copy. Non-critical -- if this rename somehow
                    // fails, the displaced old binary simply remains at the
                    // temp path instead of at .old.
                    let _ = fs::rename(&p.temp, &old);
                    Ok(())
                }
                Err(e) if is_exchange_unsupported(&e) => {
                    fs::rename(&self.exe, &old)?;
                    fs::rename(&p.temp, &self.exe)?;
                    Ok(())
                }
                // Any other exchange error: the current binary was never
                // touched (RENAME_EXCHANGE either fully succeeds or has no
                // effect), so this is a plain refusal.
                Err(e) => Err(e),
            }
        })();
        if let Err(e) = swap {
            // Best effort to restore: only reachable from the fallback
            // two-rename branch (exchange itself never partially applies),
            // where the current binary may already have been moved to .old
            // while the temp rename failed.
            if !self.exe.exists() && old.exists() {
                let _ = fs::rename(&old, &self.exe);
            }
            let _ = fs::remove_file(&p.temp);
            return Err(format!("swap: {e}"));
        }
        Ok(Some(Staged {
            version: p.version,
            command_id: p.command_id,
            exe: self.exe.clone(),
        }))
    }

    fn abort(&mut self) {
        if let Some(p) = self.pending.take() {
            let _ = fs::remove_file(&p.temp);
        }
    }

    fn temp_path(&self) -> PathBuf {
        self.exe.with_file_name(format!(
            ".{}.update",
            self.exe.file_name().unwrap_or_default().to_string_lossy()
        ))
    }

    fn old_path(&self) -> PathBuf {
        self.exe.with_file_name(format!(
            "{}.old",
            self.exe.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
}

/// Atomically exchange two paths (Linux renameat2 RENAME_EXCHANGE): both
/// files swap places in one syscall, so the exe path is never empty.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let ca = std::ffi::CString::new(a.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let cb = std::ffi::CString::new(b.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Kernel/filesystem doesn't support `RENAME_EXCHANGE` -- fall back to the
/// two-rename sequence rather than treating this as a refusal.
fn is_exchange_unsupported(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ENOTSUP)
    )
}

/// Replace this process with `exe`, preserving the original argv and env --
/// pid is preserved, so a supervising systemd unit never notices. Returns
/// only if exec itself failed.
pub fn reexec(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    std::process::Command::new(exe).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch dir per test; contains a fake "current" agent binary.
    fn scratch() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "argus-update-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("argus-agent");
        fs::write(&exe, b"OLD-BINARY").unwrap();
        (dir, exe)
    }

    fn hex_sha256(data: &[u8]) -> String {
        let d = ring::digest::digest(&ring::digest::SHA256, data);
        d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn stages_a_valid_update_and_keeps_old() {
        let (dir, exe) = scratch();
        let new_bin = b"NEW-BINARY-CONTENTS".to_vec();
        let mut u = Updater::new(exe.clone());
        u.begin(
            "9.9.9",
            &hex_sha256(&new_bin),
            new_bin.len() as u64,
            "cmd-1",
        )
        .unwrap();
        let staged = u.chunk(&new_bin, true).unwrap().expect("staged");
        assert_eq!(staged.version, "9.9.9");
        assert_eq!(staged.command_id, "cmd-1");
        assert_eq!(staged.exe, exe);
        // New binary in place, old preserved beside it, temp gone.
        assert_eq!(fs::read(&exe).unwrap(), new_bin);
        assert_eq!(
            fs::read(dir.join("argus-agent.old")).unwrap(),
            b"OLD-BINARY"
        );
        assert!(!dir.join(".argus-agent.update").exists());
        // Executable bit set.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o111,
            0o111
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn multi_chunk_assembly() {
        let (dir, exe) = scratch();
        let new_bin: Vec<u8> = (0..=255u8).cycle().take(700_000).collect();
        let mut u = Updater::new(exe.clone());
        u.begin(
            "9.9.9",
            &hex_sha256(&new_bin),
            new_bin.len() as u64,
            "cmd-2",
        )
        .unwrap();
        assert!(u.chunk(&new_bin[..256 * 1024], false).unwrap().is_none());
        assert!(u
            .chunk(&new_bin[256 * 1024..512 * 1024], false)
            .unwrap()
            .is_none());
        let staged = u
            .chunk(&new_bin[512 * 1024..], true)
            .unwrap()
            .expect("staged");
        assert_eq!(staged.version, "9.9.9");
        assert_eq!(fs::read(&exe).unwrap(), new_bin);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sha_mismatch_refuses_and_leaves_current_untouched() {
        let (dir, exe) = scratch();
        let new_bin = b"NEW-BINARY".to_vec();
        let mut u = Updater::new(exe.clone());
        u.begin(
            "9.9.9",
            &hex_sha256(b"different bytes"),
            new_bin.len() as u64,
            "c",
        )
        .unwrap();
        let err = u.chunk(&new_bin, true).unwrap_err();
        assert!(err.contains("sha256"), "got: {err}");
        assert_eq!(fs::read(&exe).unwrap(), b"OLD-BINARY");
        assert!(!dir.join(".argus-agent.update").exists());
        assert!(!dir.join("argus-agent.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn size_overrun_and_underrun_refuse() {
        let (dir, exe) = scratch();
        let mut u = Updater::new(exe.clone());
        u.begin("9", &hex_sha256(b"xx"), 2, "c").unwrap();
        assert!(u.chunk(b"xxx", true).unwrap_err().contains("size"));
        // Fresh begin after a refusal must work (state cleared).
        u.begin("9", &hex_sha256(b"xx"), 2, "c").unwrap();
        assert!(u.chunk(b"x", true).unwrap_err().contains("size"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_concurrent_and_implausible() {
        let (dir, exe) = scratch();
        let mut u = Updater::new(exe.clone());
        assert!(u.begin("9", "00", 0, "c").is_err()); // zero bytes
        assert!(u.begin("9", "00", 65 * 1024 * 1024, "c").is_err()); // > 64 MiB cap
        u.begin("9", "00", 10, "c").unwrap();
        assert!(u
            .begin("9", "00", 10, "c")
            .unwrap_err()
            .contains("in flight"));
        fs::remove_dir_all(dir).unwrap();
    }
}
