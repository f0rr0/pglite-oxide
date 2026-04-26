pub mod bench;

use serde::ser::Serializer;
use tauri::Manager;

use bench::{BenchReport, BenchState};

#[derive(Debug, thiserror::Error)]
enum CommandError {
    #[error("{0}")]
    Runtime(String),
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(value: anyhow::Error) -> Self {
        Self::Runtime(format!("{value:#}"))
    }
}

#[tauri::command]
async fn profile_queries(
    state: tauri::State<'_, BenchState>,
    fresh: bool,
    row_count: u32,
) -> Result<BenchReport, CommandError> {
    state
        .profile_queries(fresh, row_count)
        .await
        .map_err(CommandError::from)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let root = app
                .path()
                .app_data_dir()
                .map(|dir| dir.join("pglite-sqlx-profile"))?;
            app.manage(BenchState::new(root));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![profile_queries])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
