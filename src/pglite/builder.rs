use std::path::PathBuf;

use anyhow::{Result, bail};
use tempfile::TempDir;

use crate::pglite::base::{
    PglitePaths, RootLock, install_into_without_template, install_paths,
    install_paths_without_template, install_temporary_from_template,
};
use crate::pglite::client::Pglite;
#[cfg(feature = "extensions")]
use crate::pglite::extensions::Extension;

/// Builder for opening persistent or temporary [`Pglite`] databases.
#[derive(Debug, Clone)]
pub struct PgliteBuilder {
    target: Option<PgliteTarget>,
    template_cache: bool,
    #[cfg(feature = "extensions")]
    extensions: Vec<Extension>,
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
            #[cfg(feature = "extensions")]
            extensions: Vec::new(),
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

    /// Control whether new databases are cloned from the process-local or
    /// embedded PGDATA template cache.
    pub fn template_cache(mut self, enabled: bool) -> Self {
        self.template_cache = enabled;
        self
    }

    /// Open an ephemeral database with a fresh `initdb`.
    pub fn fresh_temporary(self) -> Self {
        self.temporary().template_cache(false)
    }

    /// Enable a bundled Postgres extension before returning the database.
    #[cfg(feature = "extensions")]
    pub fn extension(mut self, extension: Extension) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Enable bundled Postgres extensions before returning the database.
    #[cfg(feature = "extensions")]
    pub fn extensions(mut self, extensions: impl IntoIterator<Item = Extension>) -> Self {
        self.extensions.extend(extensions);
        self
    }

    /// Install, initialize, and start the selected database.
    pub fn open(self) -> Result<Pglite> {
        match self.target.clone() {
            Some(PgliteTarget::Path(root)) => {
                let paths = PglitePaths::with_root(&root);
                let lock = RootLock::acquire(&root)?;
                let outcome = if self.template_cache {
                    install_paths(paths)?
                } else {
                    install_paths_without_template(paths)?
                };
                self.open_paths(outcome.paths, Some(lock))
            }
            Some(PgliteTarget::AppId {
                qualifier,
                organization,
                application,
            }) => {
                let paths = PglitePaths::new((&qualifier, &organization, &application))?;
                let lock = RootLock::acquire_for_paths(&paths)?;
                let outcome = if self.template_cache {
                    install_paths(paths)?
                } else {
                    install_paths_without_template(paths)?
                };
                self.open_paths(outcome.paths, Some(lock))
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
            let outcome = install_into_without_template(temp_dir.path())?;
            (temp_dir, outcome)
        };

        let mut instance = self.open_paths(outcome.paths, None)?;
        instance.attach_temp_dir(temp_dir);
        Ok(instance)
    }

    fn open_paths(self, paths: PglitePaths, root_lock: Option<RootLock>) -> Result<Pglite> {
        let mut instance = Pglite::new(paths)?;
        if let Some(lock) = root_lock {
            instance.attach_root_lock(lock);
        }
        #[cfg(feature = "extensions")]
        let mut instance = instance;
        #[cfg(feature = "extensions")]
        for extension in self.extensions {
            instance.enable_extension(extension)?;
        }
        Ok(instance)
    }
}
