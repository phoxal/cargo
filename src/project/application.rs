//! Embedded application interface inspection; never executes foreign code.
use super::Error;
use object::{Object, ObjectSection};
use phoxal::artifact::application::{self, ApplicationContract};
use std::path::Path;
pub(crate) fn read_embedded_contract(path: &Path) -> Result<ApplicationContract<String>, Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })?;
    let file = object::File::parse(bytes.as_slice()).map_err(|error| Error::SupervisorLaunch {
        message: format!(
            "application executable {} is not a readable binary: {error}",
            path.display()
        ),
    })?;
    for section in file.sections() {
        let name = section.name().unwrap_or_default();
        if !application::APPLICATION_SECTION_NAMES.contains(&name) {
            continue;
        }
        let data = section
            .uncompressed_data()
            .map_err(|error| Error::SupervisorLaunch {
                message: format!(
                    "cannot read the application contract of {}: {error}",
                    path.display()
                ),
            })?;
        return application::decode_application_contract(&data)
            .map(ApplicationContract::into_owned)
            .ok_or_else(|| Error::SupervisorLaunch {
                message: format!(
                    "application executable {} carries a malformed application contract record",
                    path.display()
                ),
            });
    }
    Err(Error::SupervisorLaunch {
        message: format!(
            "application executable {} carries no embedded application contract",
            path.display()
        ),
    })
}
