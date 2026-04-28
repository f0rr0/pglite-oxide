/// A bundled Postgres extension that can be installed into a PGlite database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extension {
    name: &'static str,
    sql_name: &'static str,
    schema: &'static str,
    archive_name: &'static str,
    aot_name: &'static str,
}

impl Extension {
    #[allow(dead_code)]
    pub(crate) const fn new(
        name: &'static str,
        sql_name: &'static str,
        schema: &'static str,
        archive_name: &'static str,
        aot_name: &'static str,
    ) -> Self {
        Self {
            name,
            sql_name,
            schema,
            archive_name,
            aot_name,
        }
    }

    /// Human-facing extension name.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// SQL extension name used in `CREATE EXTENSION`.
    pub const fn sql_name(self) -> &'static str {
        self.sql_name
    }

    /// Schema used when creating the extension inside bundled PGlite.
    pub const fn schema(self) -> &'static str {
        self.schema
    }

    /// Archive path inside the asset manifest.
    pub const fn archive_name(self) -> &'static str {
        self.archive_name
    }

    /// AOT artifact key for the extension side module.
    pub const fn aot_name(self) -> &'static str {
        self.aot_name
    }
}

pub const VECTOR: Extension = Extension::new(
    "pgvector",
    "vector",
    "pg_catalog",
    "extensions/vector.tar.zst",
    "extension:vector",
);

pub const PG_TRGM: Extension = Extension::new(
    "pg_trgm",
    "pg_trgm",
    "pg_catalog",
    "extensions/pg_trgm.tar.zst",
    "extension:pg_trgm",
);

pub const ALL: &[Extension] = &[VECTOR, PG_TRGM];

pub fn by_sql_name(sql_name: &str) -> Option<Extension> {
    ALL.iter()
        .copied()
        .find(|extension| extension.sql_name == sql_name)
}

pub(crate) fn create_extension_sql(extension: Extension) -> String {
    format!(
        "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {};",
        crate::pglite::templating::quote_identifier(extension.sql_name()),
        crate::pglite::templating::quote_identifier(extension.schema())
    )
}
