use std::path::PathBuf;

use anyhow::{Result, bail};
use tempfile::TempDir;

use crate::pglite::base::{install_default, install_into, install_temporary_from_template};
use crate::pglite::client::Pglite;

/// Builder for opening persistent or temporary [`Pglite`] databases.
#[derive(Debug, Clone)]
pub struct PgliteBuilder {
    target: Option<PgliteTarget>,
    template_cache: bool,
}

#[derive(Debug, Clone)]
enum PgliteTarget {
    Path(PathBuf),
    AppId {
        qualifier: String,
        organization: String,
        application: String,
    },
    Temporary,
}

impl Default for PgliteBuilder {
    fn default() -> Self {
        Self {
            target: None,
            template_cache: true,
        }
    }
}

impl PgliteBuilder {
    /// Create a builder. Call [`path`](Self::path), [`app_id`](Self::app_id),
    /// or [`temporary`](Self::temporary) before [`open`](Self::open).
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a persistent database rooted at `root`.
    pub fn path(mut self, root: impl Into<PathBuf>) -> Self {
        self.target = Some(PgliteTarget::Path(root.into()));
        self
    }

    /// Open a persistent database under the platform data directory.
    pub fn app(
        mut self,
        qualifier: impl Into<String>,
        organization: impl Into<String>,
        application: impl Into<String>,
    ) -> Self {
        self.target = Some(PgliteTarget::AppId {
            qualifier: qualifier.into(),
            organization: organization.into(),
            application: application.into(),
        });
        self
    }

    /// Open a persistent database under the platform data directory.
    pub fn app_id(self, app_id: (&str, &str, &str)) -> Self {
        self.app(app_id.0, app_id.1, app_id.2)
    }

    /// Open an ephemeral database removed when the instance is dropped.
    ///
    /// Temporary databases use the process-local template cluster cache by
    /// default, avoiding repeated `initdb` work in test suites.
    pub fn temporary(mut self) -> Self {
        self.target = Some(PgliteTarget::Temporary);
        self
    }

    /// Control whether temporary databases are cloned from the process-local
    /// template cluster cache.
    pub fn template_cache(mut self, enabled: bool) -> Self {
        self.template_cache = enabled;
        self
    }

    /// Open an ephemeral database with a fresh `initdb`.
    pub fn fresh_temporary(self) -> Self {
        self.temporary().template_cache(false)
    }

    /// Install, initialize, and start the selected database.
    pub fn open(self) -> Result<Pglite> {
        match self.target {
            Some(PgliteTarget::Path(root)) => {
                let outcome = install_into(&root)?;
                Pglite::new(outcome.paths)
            }
            Some(PgliteTarget::AppId {
                qualifier,
                organization,
                application,
            }) => {
                let outcome = install_default((&qualifier, &organization, &application))?;
                Pglite::new(outcome.paths)
            }
            Some(PgliteTarget::Temporary) => self.open_temporary(),
            None => {
                bail!(
                    "PgliteBuilder target is not set; call path, app_id, or temporary before open"
                )
            }
        }
    }

    fn open_temporary(self) -> Result<Pglite> {
        let (temp_dir, outcome) = if self.template_cache {
            install_temporary_from_template()?
        } else {
            let temp_dir = TempDir::new()?;
            let outcome = install_into(temp_dir.path())?;
            (temp_dir, outcome)
        };

        let mut instance = Pglite::new(outcome.paths)?;
        instance.attach_temp_dir(temp_dir);
        Ok(instance)
    }
}
