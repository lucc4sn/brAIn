//! Cópia sincronizada das definições de subagentes: `~/.claude/agents` (somente
//! leitura) → `~/.claude_copy/agents`. Nunca apaga nada: o que sobra no destino
//! é apenas reportado em `stale`.
//!
//! A cópia é DESCARTÁVEL: edições locais nela são sobrescritas quando a origem
//! muda (tamanho ou mtime). Edite sempre a origem.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::fs_util::{is_unsafe_char, write_atomic};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_FILES: usize = 200;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("diretório home indisponível")]
    NoHome,
    #[error("o destino da cópia é (ou está sob) um link simbólico: {0}")]
    SymlinkTarget(String),
    #[error("erro de E/S em {path}: {source}")]
    Io { path: String, source: io::Error },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SyncError + '_ {
    move |source| SyncError::Io { path: path.display().to_string(), source }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub copied: usize,
    pub unchanged: usize,
    pub skipped: Vec<String>,
    /// Existem no destino, mas não na origem (nada é apagado).
    pub stale: Vec<String>,
    pub source_missing: bool,
}

/// `~/.claude/agents`.
pub fn source_dir() -> Result<PathBuf, SyncError> {
    dirs::home_dir().map(|h| h.join(".claude").join("agents")).ok_or(SyncError::NoHome)
}

/// `~/.claude_copy/agents`.
pub fn copy_dir() -> Result<PathBuf, SyncError> {
    dirs::home_dir().map(|h| h.join(".claude_copy").join("agents")).ok_or(SyncError::NoHome)
}

/// Sincroniza a origem padrão com o destino padrão.
pub fn sync_agents() -> Result<SyncReport, SyncError> {
    sync_dirs(&source_dir()?, &copy_dir()?)
}

/// Nome simples de definição: `*.md`, sem separadores nem caracteres de controle/invisíveis.
fn valid_name(name: &str) -> bool {
    name.len() > ".md".len()
        && name.ends_with(".md")
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(is_unsafe_char)
}

/// Nome de um arquivo regular `*.md` (sem seguir symlinks) ou `None`.
fn regular_md(entry: &fs::DirEntry) -> Option<(String, fs::Metadata)> {
    let name = entry.file_name().into_string().ok()?;
    let meta = entry.metadata().ok().filter(fs::Metadata::is_file)?;
    (name.ends_with(".md")).then_some((name, meta))
}

/// Pura (parametrizada por `src`/`dst`). Copia `*.md` de nível único quando o
/// destino não existe ou difere em tamanho/mtime; depois alinha o mtime do
/// destino ao da origem.
pub fn sync_dirs(src: &Path, dst: &Path) -> Result<SyncReport, SyncError> {
    let mut report = SyncReport::default();
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            report.source_missing = true;
            return Ok(report);
        }
        Err(e) => return Err(io_err(src)(e)),
    };
    let mut files: Vec<(String, fs::Metadata)> = entries.filter_map(Result::ok).filter_map(|e| regular_md(&e)).collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for p in [dst.parent(), Some(dst)].into_iter().flatten() {
        if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(SyncError::SymlinkTarget(p.display().to_string()));
        }
    }
    fs::create_dir_all(dst).map_err(io_err(dst))?;

    let mut source_names = BTreeSet::new();
    let mut considered = 0usize;
    for (name, meta) in files {
        source_names.insert(name.clone());
        if !valid_name(&name) {
            report.skipped.push(format!("{}: nome inválido", name.escape_debug()));
            continue;
        }
        if considered >= MAX_FILES {
            report.skipped.push(format!("{name}: acima do limite de {MAX_FILES} arquivos"));
            continue;
        }
        considered += 1;
        if meta.len() > MAX_FILE_BYTES {
            report.skipped.push(format!("{name}: maior que {} KiB", MAX_FILE_BYTES / 1024));
            continue;
        }
        match copy_one(&src.join(&name), &dst.join(&name), &meta) {
            Ok(true) => report.copied += 1,
            Ok(false) => report.unchanged += 1,
            Err(why) => report.skipped.push(format!("{name}: {why}")),
        }
    }

    if let Ok(entries) = fs::read_dir(dst) {
        let mut stale: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| regular_md(&e).map(|(n, _)| n))
            .filter(|n| !source_names.contains(n))
            .collect();
        stale.sort();
        report.stale = stale;
    }
    Ok(report)
}

/// `Ok(true)` copiou; `Ok(false)` já estava igual; `Err(motivo)` não copiou.
fn copy_one(from: &Path, to: &Path, src_meta: &fs::Metadata) -> Result<bool, String> {
    let src_mtime = src_meta.modified().map_err(|e| format!("mtime da origem indisponível: {e}"))?;
    match fs::symlink_metadata(to) {
        Ok(m) if !m.is_file() => return Err("destino existe e não é arquivo regular; não sobrescrito".into()),
        Ok(m) if m.len() == src_meta.len() && m.modified().is_ok_and(|t| t == src_mtime) => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("destino ilegível: {e}")),
    }
    let mut bytes = Vec::new();
    fs::File::open(from)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("leitura falhou: {e}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("maior que {} KiB", MAX_FILE_BYTES / 1024));
    }
    write_atomic(to, &bytes, 0o644).map_err(|e| format!("escrita falhou: {e}"))?;
    OpenOptions::new()
        .write(true)
        .open(to)
        .and_then(|f| f.set_modified(src_mtime))
        .map_err(|e| format!("ajuste de mtime falhou: {e}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    struct Env {
        _td: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
    }

    fn env() -> Env {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("src");
        let dst = td.path().join("copia/agents");
        fs::create_dir_all(&src).unwrap();
        Env { _td: td, src, dst }
    }

    #[test]
    fn copia_depois_estabiliza() {
        let e = env();
        fs::write(e.src.join("a.md"), "alfa").unwrap();
        fs::write(e.src.join("b.md"), "beta").unwrap();
        fs::write(e.src.join("nota.txt"), "x").unwrap();
        fs::create_dir(e.src.join("sub.md")).unwrap();
        fs::create_dir(e.src.join("sub")).unwrap();
        fs::write(e.src.join("sub/c.md"), "c").unwrap();
        symlink(e.src.join("a.md"), e.src.join("link.md")).unwrap();
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!((r.copied, r.unchanged), (2, 0), "{r:?}");
        assert!(r.skipped.is_empty() && r.stale.is_empty() && !r.source_missing);
        assert_eq!(fs::read_to_string(e.dst.join("a.md")).unwrap(), "alfa");
        assert!(!e.dst.join("c.md").exists() && !e.dst.join("link.md").exists() && !e.dst.join("nota.txt").exists());
        assert_eq!(
            fs::metadata(e.src.join("a.md")).unwrap().modified().unwrap(),
            fs::metadata(e.dst.join("a.md")).unwrap().modified().unwrap()
        );
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!((r.copied, r.unchanged), (0, 2));
    }

    #[test]
    fn recopia_quando_muda_tamanho_ou_mtime() {
        let e = env();
        fs::write(e.src.join("a.md"), "um").unwrap();
        sync_dirs(&e.src, &e.dst).unwrap();
        fs::write(e.src.join("a.md"), "dois").unwrap();
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(fs::read_to_string(e.dst.join("a.md")).unwrap(), "dois");
        // mesmo tamanho, mtime diferente
        fs::write(e.src.join("a.md"), "tres").unwrap();
        let f = OpenOptions::new().write(true).open(e.src.join("a.md")).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        drop(f);
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(fs::read_to_string(e.dst.join("a.md")).unwrap(), "tres");
    }

    #[test]
    fn nunca_apaga_e_reporta_stale() {
        let e = env();
        fs::write(e.src.join("a.md"), "a").unwrap();
        fs::create_dir_all(&e.dst).unwrap();
        fs::write(e.dst.join("velho.md"), "meu").unwrap();
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.stale, vec!["velho.md".to_string()]);
        assert_eq!(fs::read_to_string(e.dst.join("velho.md")).unwrap(), "meu");
        // a origem nunca é modificada
        assert_eq!(fs::read_to_string(e.src.join("a.md")).unwrap(), "a");
    }

    #[test]
    fn origem_ausente_nao_e_erro_e_nao_cria_destino() {
        let e = env();
        let r = sync_dirs(&e.src.join("nao-existe"), &e.dst).unwrap();
        assert!(r.source_missing);
        assert!(!e.dst.exists());
    }

    #[test]
    fn limites_de_tamanho_e_quantidade() {
        let e = env();
        fs::write(e.src.join("grande.md"), vec![b'x'; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        fs::write(e.src.join("limite.md"), vec![b'x'; MAX_FILE_BYTES as usize]).unwrap();
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].starts_with("grande.md"));

        let e = env();
        for i in 0..205 {
            fs::write(e.src.join(format!("a{i:03}.md")), "x").unwrap();
        }
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.copied, MAX_FILES);
        assert_eq!(r.skipped.len(), 5);
    }

    #[test]
    fn destino_nao_regular_nao_e_sobrescrito() {
        let e = env();
        fs::write(e.src.join("a.md"), "a").unwrap();
        fs::create_dir_all(&e.dst).unwrap();
        let alvo = e.dst.parent().unwrap().join("alvo.txt");
        fs::write(&alvo, "intacto").unwrap();
        symlink(&alvo, e.dst.join("a.md")).unwrap();
        let r = sync_dirs(&e.src, &e.dst).unwrap();
        assert_eq!(r.copied, 0);
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(fs::read_to_string(&alvo).unwrap(), "intacto");
    }

    #[test]
    fn nomes_validos() {
        for ok in ["a.md", "code-reviewer.md", "ação.md"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [".md", "a/b.md", "a\\b.md", "a\nb.md", "a\u{202e}b.md", "a.txt", ""] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn relatorio_serializa_em_camel_case() {
        let v = serde_json::to_value(SyncReport::default()).unwrap();
        let mut ks: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        ks.sort();
        assert_eq!(ks, vec!["copied", "skipped", "sourceMissing", "stale", "unchanged"]);
    }

    #[test]
    fn grant_policy_permite_a_copia_e_proibe_o_original() {
        use crate::bases::{GrantPolicy, grant_policy};
        let home = Path::new("/home/u");
        assert_eq!(grant_policy(&home.join(".claude_copy/agents"), Some(home)), GrantPolicy::Allowed);
        assert!(matches!(grant_policy(&home.join(".claude/agents"), Some(home)), GrantPolicy::Forbidden(_)));
        assert!(matches!(grant_policy(&home.join(".claude"), Some(home)), GrantPolicy::Forbidden(_)));
    }

    #[test]
    fn destino_symlink_e_recusado_sem_escrever() {
        let e = env();
        fs::write(e.src.join("a.md"), "a").unwrap();
        let real = e.dst.parent().unwrap().parent().unwrap().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = e.dst.parent().unwrap().parent().unwrap().join("link");
        symlink(&real, &link).unwrap();
        assert!(matches!(sync_dirs(&e.src, &link), Err(SyncError::SymlinkTarget(_))));
        assert!(matches!(sync_dirs(&e.src, &link.join("agents")), Err(SyncError::SymlinkTarget(_))));
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }
}
