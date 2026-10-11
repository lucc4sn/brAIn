//! Leitura direta de `.git/HEAD` (sem crate git). Qualquer falha vira `None`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHead {
    Branch(String),
    /// Os 7 primeiros caracteres do hash.
    Detached(String),
}

impl GitHead {
    /// Texto exibido no front (`git_branch`).
    pub fn label(&self) -> String {
        match self {
            GitHead::Branch(name) => name.clone(),
            GitHead::Detached(hash) => format!("detached@{hash}"),
        }
    }
}

const MAX_HEAD_BYTES: u64 = 4096;

/// Lê no máximo 4 KiB (nunca o arquivo inteiro).
fn read_bounded(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(MAX_HEAD_BYTES).read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Resolve o diretório git real: `.git` pode ser pasta ou arquivo `gitdir: <path>`.
fn resolve_git_dir(project_dir: &Path) -> Option<PathBuf> {
    let dot_git = project_dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let content = read_bounded(&dot_git)?;
    let target = content.lines().next()?.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        project_dir.join(target)
    })
}

pub fn read_branch(project_dir: &Path) -> Option<GitHead> {
    let git_dir = resolve_git_dir(project_dir)?;
    let head = read_bounded(&git_dir.join("HEAD"))?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return (!name.is_empty()).then(|| GitHead::Branch(name.to_string()));
    }
    if matches!(head.len(), 40 | 64) && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(GitHead::Detached(head.chars().take(7).collect()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo(head: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD"), head).unwrap();
        dir
    }

    #[test]
    fn le_branch() {
        let d = repo("ref: refs/heads/feature/x\n");
        assert_eq!(read_branch(d.path()), Some(GitHead::Branch("feature/x".into())));
    }

    #[test]
    fn le_detached() {
        let d = repo("0123456789abcdef0123456789abcdef01234567\n");
        let h = read_branch(d.path()).unwrap();
        assert_eq!(h, GitHead::Detached("0123456".into()));
        assert_eq!(h.label(), "detached@0123456");
    }

    #[test]
    fn git_como_arquivo_relativo_e_absoluto() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("store/wt");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        let proj = d.path().join("proj");
        fs::create_dir(&proj).unwrap();
        fs::write(proj.join(".git"), "gitdir: ../store/wt\n").unwrap();
        assert_eq!(read_branch(&proj), Some(GitHead::Branch("wt".into())));
        fs::write(proj.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        assert_eq!(read_branch(&proj), Some(GitHead::Branch("wt".into())));
    }

    #[test]
    fn falhas_viram_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_branch(d.path()), None);
        fs::write(d.path().join(".git"), "lixo").unwrap();
        assert_eq!(read_branch(d.path()), None);
        let d2 = repo("zzz\n");
        assert_eq!(read_branch(d2.path()), None);
    }
}
