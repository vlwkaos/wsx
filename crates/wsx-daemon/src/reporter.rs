//! Upgrade-stable reporter entry point for panes that survive daemon handoff.
//! See docs/agent-reporting.md. The daemon publishes only after it owns the runtime.
use std::fs;
use std::io;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

pub(crate) struct Publication {
    temporary: PathBuf,
    destination: PathBuf,
}

impl Publication {
    // ^ [[Agent Reporter Lifetime]] Keep adapter recovery in sync with docs/agent-reporting.md.
    pub(crate) fn prepare(socket: &Path, executable: &Path) -> io::Result<Self> {
        let destination = socket.with_extension("reporter");
        super::secure_parent(&destination)?;
        validate_entry(&destination)?;
        let binary = executable
            .parent()
            .ok_or_else(|| io::Error::other("daemon executable has no parent"))?
            .join("wsx")
            .canonicalize()?;
        let metadata = fs::metadata(&binary)?;
        if !metadata.is_file()
            || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe reporter executable",
            ));
        }
        let temporary = destination.with_extension(format!("reporter.tmp.{}", std::process::id()));
        // create-new semantics: never follow or overwrite a pre-existing staging path.
        symlink(binary, &temporary)?;
        Ok(Self {
            temporary,
            destination,
        })
    }

    fn publish(&self) -> io::Result<()> {
        validate_entry(&self.destination)?;
        fs::rename(&self.temporary, &self.destination)
    }
}

pub(crate) fn publish_pending(pending: &mut Option<Publication>) -> io::Result<()> {
    if let Some(publication) = pending.as_ref() {
        publication.publish()?;
    }
    *pending = None;
    Ok(())
}

fn validate_entry(destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() } =>
        {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe existing reporter entry",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    #[test]
    fn failed_publication_retains_stage_and_retries_without_overwriting_reserved_content() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../.work/reporter-retry-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("wsx"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(root.join("wsx"), fs::Permissions::from_mode(0o755)).unwrap();
        let socket = root.join("wsx.sock");
        let reporter = socket.with_extension("reporter");
        let mut pending = Some(Publication::prepare(&socket, &root.join("wsxd")).unwrap());
        fs::write(&reporter, "unrelated content created after staging").unwrap();
        assert!(publish_pending(&mut pending).is_err());
        assert!(pending.as_ref().unwrap().temporary.exists());
        assert_eq!(
            fs::read_to_string(&reporter).unwrap(),
            "unrelated content created after staging"
        );
        fs::remove_file(&reporter).unwrap();
        publish_pending(&mut pending).unwrap();
        assert!(pending.is_none());
        assert!(Command::new(&reporter).status().unwrap().success());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_survives_old_install_removal_and_aborted_handoff() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../.work/reporter-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("wsx.sock");
        for version in ["old", "new"] {
            let directory = root.join(version);
            fs::create_dir_all(&directory).unwrap();
            let binary = directory.join("wsx");
            fs::write(&binary, format!("#!/bin/sh\nprintf '%s' {version}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = root.join("old/wsxd");
        let new = root.join("new/wsxd");
        let reporter = socket.with_extension("reporter");
        let output =
            || String::from_utf8(Command::new(&reporter).output().unwrap().stdout).unwrap();
        Publication::prepare(&socket, &old)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(output(), "old");
        drop(Publication::prepare(&socket, &new).unwrap());
        assert_eq!(output(), "old", "an aborted successor must not publish");
        let prepared = Publication::prepare(&socket, &new).unwrap();
        assert_eq!(output(), "old", "staging must retain the current owner");
        prepared.publish().unwrap();
        drop(prepared);
        fs::remove_file(root.join("old/wsx")).unwrap();
        assert_eq!(output(), "new");
        fs::set_permissions(root.join("new/wsx"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Publication::prepare(&socket, &new).is_err());
        fs::set_permissions(root.join("new/wsx"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(&reporter).unwrap();
        fs::write(&reporter, "unrelated reserved-path content").unwrap();
        assert!(Publication::prepare(&socket, &new).is_err());
        assert_eq!(
            fs::read_to_string(&reporter).unwrap(),
            "unrelated reserved-path content"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
