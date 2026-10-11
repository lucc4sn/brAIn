//! Comandos Tauri finos: convertem args, chamam o módulo, devolvem o resultado.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::agents_sync::{self, SyncReport};
use crate::bases::{self, ResolutionView};
use crate::config::{Config, LoadedConfig};
use crate::error::AppError;
use crate::launcher::{self, LaunchMode, LaunchOutcome, TerminalStatus};
use crate::sessions::{self, ListScope, SessionsResult};
use crate::tree::{self, TreeResult};

/// Avisos da carga ainda não entregues ao front.
#[derive(Default)]
struct Pending {
    recovered: bool,
    warnings: Vec<String>,
}

/// A configuração é carregada UMA vez (no startup) e vive aqui.
#[derive(Default)]
pub struct AppState {
    pub known_projects: Mutex<Vec<String>>,
    config: Mutex<Config>,
    pending: Mutex<Pending>,
}

/// Um lock envenenado não derruba o app: usa o valor interno mesmo assim.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AppState {
    pub fn from_loaded(loaded: LoadedConfig) -> Self {
        AppState {
            known_projects: Mutex::new(Vec::new()),
            config: Mutex::new(loaded.config),
            pending: Mutex::new(Pending { recovered: loaded.recovered, warnings: loaded.warnings }),
        }
    }

    /// Carrega do disco; se falhar, segue com os padrões e avisa no 1º `get_config`.
    pub fn load() -> Self {
        match Config::load() {
            Ok(loaded) => Self::from_loaded(loaded),
            Err(e) => Self::from_loaded(LoadedConfig {
                config: Config::default(),
                recovered: false,
                warnings: vec![format!("configuração não carregada, usando padrões: {e}")],
            }),
        }
    }

    fn config(&self) -> Config {
        lock(&self.config).clone()
    }

    fn known(&self) -> Vec<String> {
        lock(&self.known_projects).clone()
    }
}

async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Task(e.to_string()))?
}

/// Devolve a config do estado e os avisos pendentes (que são limpos).
#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> LoadedConfig {
    let pending = std::mem::take(&mut *lock(&state.pending));
    LoadedConfig { config: state.config(), recovered: pending.recovered, warnings: pending.warnings }
}

#[tauri::command]
pub async fn save_config(state: tauri::State<'_, AppState>, config: Config) -> Result<(), AppError> {
    // O estado guarda a versão normalizada (chaves de `projects` canônicas).
    let normalized = config.normalized();
    let to_save = normalized.clone();
    blocking(move || Ok(to_save.save()?)).await?;
    *lock(&state.config) = normalized;
    Ok(())
}

#[tauri::command]
pub async fn scan_tree(state: tauri::State<'_, AppState>) -> Result<TreeResult, AppError> {
    let config = state.config();
    let result = blocking(move || {
        let dir = sessions::claude_projects_dir()?;
        Ok(tree::scan_tree(&config, &dir, sessions::now_ms())?)
    })
    .await?;
    *lock(&state.known_projects) = result.nodes.iter().map(|n| n.path.clone()).collect();
    Ok(result)
}

#[tauri::command]
pub async fn list_sessions(
    state: tauri::State<'_, AppState>,
    project_path: String,
) -> Result<SessionsResult, AppError> {
    let config = state.config();
    let known: Vec<PathBuf> = state.known().into_iter().map(PathBuf::from).collect();
    blocking(move || {
        let dir = sessions::claude_projects_dir()?;
        let project = PathBuf::from(project_path);
        let scope = ListScope { known: &known, aggregate_children: sessions::aggregates_children(&config, &project) };
        Ok(sessions::list_sessions_for(&dir, &project, scope)?)
    })
    .await
}

#[tauri::command]
pub async fn launch(
    state: tauri::State<'_, AppState>,
    project_path: String,
    mode: LaunchMode,
) -> Result<LaunchOutcome, AppError> {
    let known = state.known();
    let config = state.config();
    blocking(move || {
        // Mantém a cópia dos agentes em dia; falha não impede o launch.
        if let Err(e) = agents_sync::sync_agents() {
            eprintln!("brain: sincronização de agentes falhou: {e}");
        }
        Ok(launcher::launch_project_with_bases(&PathBuf::from(project_path), mode, &known, &config)?)
    })
    .await
}

/// Bases de contexto que valem para a pasta (herança por prefixo de caminho).
#[tauri::command]
pub async fn resolve_bases(
    state: tauri::State<'_, AppState>,
    project_path: String,
) -> Result<ResolutionView, AppError> {
    let known = state.known();
    let config = state.config();
    blocking(move || {
        let dir = launcher::validate_launch(&PathBuf::from(project_path), &LaunchMode::New, &known)?;
        Ok(ResolutionView::from(&bases::resolve_bases(&config, &dir)))
    })
    .await
}

#[tauri::command]
pub fn detect_terminals() -> Vec<TerminalStatus> {
    launcher::detect_terminals()
}

#[tauri::command]
pub fn default_config() -> Config {
    Config::default()
}

/// Sincroniza `~/.claude/agents` → `~/.claude_copy/agents` (nunca apaga).
#[tauri::command]
pub async fn sync_agents() -> Result<SyncReport, AppError> {
    blocking(|| Ok(agents_sync::sync_agents()?)).await
}

/// Caminho da cópia dos agentes (para o front).
#[tauri::command]
pub fn agents_copy_dir() -> Result<String, AppError> {
    Ok(agents_sync::copy_dir()?.to_string_lossy().into_owned())
}
