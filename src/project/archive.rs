//! Portable archives of complete runnable robot directories.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zip::write::SimpleFileOptions;

use super::Error;

pub(super) fn publish(root: &Path, output: &Path) -> Result<(), Error> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let root = root.canonicalize()?;
        fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        if parent.starts_with(&root) {
            return Err("archive destination must be outside the runnable build".into());
        }
        let mut staging = tempfile::NamedTempFile::new_in(&parent)?;
        {
            let mut archive = zip::ZipWriter::new(staging.as_file_mut());
            append(&mut archive, &root, &root)?;
            archive.finish()?;
        }
        staging.as_file_mut().sync_all()?;
        staging.persist(output)?;
        Ok(())
    })();
    result.map_err(|error| Error::InvalidOptions {
        message: format!(
            "cannot archive robot build {} to {}: {error}",
            root.display(),
            output.display()
        ),
    })
}

fn append<W: io::Write + io::Seek>(
    archive: &mut zip::ZipWriter<W>,
    root: &Path,
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "runnable build contains a symbolic link: {}",
                path.display()
            )
            .into());
        }
        if metadata.is_dir() {
            append(archive, root, &path)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root)?;
            let name = relative.to_str().ok_or("archive paths must be UTF-8")?;
            archive.start_file(
                name,
                SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(metadata.permissions().mode()),
            )?;
            io::copy(&mut fs::File::open(path)?, archive)?;
        } else {
            return Err(format!(
                "runnable build contains a non-regular file: {}",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn archive_is_relative_relocatable_and_preserves_executable_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("build");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("manifest.json"), b"manifest").unwrap();
        fs::write(root.join("bin/brain"), b"executable").unwrap();
        fs::set_permissions(root.join("bin/brain"), fs::Permissions::from_mode(0o755)).unwrap();
        let output = directory.path().join("bundle/robot.zip");
        publish(&root, &output).unwrap();
        fs::remove_dir_all(root).unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        let mut executable = archive.by_name("bin/brain").unwrap();
        assert_eq!(executable.unix_mode().unwrap() & 0o777, 0o755);
        let mut bytes = Vec::new();
        executable.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"executable");
    }

    #[test]
    fn destination_inside_build_is_refused_without_changing_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("build");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("manifest.json"), b"manifest").unwrap();
        let output = root.join("robot.zip");
        fs::write(&output, b"previous").unwrap();
        assert!(publish(&root, &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"previous");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(publish(&root, &alias.join("robot.zip")).is_err());
        assert_eq!(fs::read(output).unwrap(), b"previous");
    }

    #[test]
    fn invalid_build_does_not_replace_an_existing_archive() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("build");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("escape")).unwrap();
        let output = directory.path().join("robot.zip");
        fs::write(&output, b"previous").unwrap();
        assert!(publish(&root, &output).is_err());
        assert_eq!(fs::read(output).unwrap(), b"previous");
    }
}
