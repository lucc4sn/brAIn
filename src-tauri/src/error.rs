//! Erro agregador exposto ao front como `{ kind, message }`.

use serde::{Serialize, Serializer};
use thiserror::Error;

use crate::agents_sync::SyncError;
use crate::config::ConfigError;
use crate::launcher::LaunchError;
use crate::sessions::SessionError;
use crate::tree::TreeError;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error("falha na tarefa em segundo plano: {0}")]
    Task(String),
}

impl AppError {
    pub fn kind(&self) -> &'static str {
        match self {
            AppError::Config(_) => "config",
            AppError::Session(_) => "session",
            AppError::Launch(_) => "launch",
            AppError::Tree(_) => "tree",
            AppError::Sync(_) => "sync",
            AppError::Task(_) => "task",
        }
    }
}

#[derive(Serialize)]
struct AppErrorPayload<'a> {
    kind: &'a str,
    message: String,
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AppErrorPayload {
            kind: self.kind(),
            message: self.to_string(),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializa_como_kind_e_message() {
        let err = AppError::Task("x".into());
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "task");
        assert!(v["message"].as_str().unwrap().contains('x'));
    }
}
