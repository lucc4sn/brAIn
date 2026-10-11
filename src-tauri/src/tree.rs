//! Varredura em ÁRVORE de pastas para o grafo hierárquico: todas as subpastas
//! (até um teto de profundidade e de nós) com métricas de sessões por pasta.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use walkdir::WalkDir;

use crate::config::Config;
use crate::git;
use crate::sessions::{self, session_weight, ListScope, SessionIndexCache};

const DEFAULT_MAX_NODES: usize = 5000;
/// Folga da coleta bruta antes de desistir da varredura (o corte fino é por nível).
const HARD_LIMIT_FACTOR: usize = 10;

#[derive(Debug, Error)]
pub enum TreeError {
    #[error("nenhuma raiz utilizável para varrer")]
    NoRoots,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub id: String,
    pub path: String,
    pub parent: Option<String>,
    pub name: String,
    pub depth: u32,
    pub is_project: bool,
    pub has_git: bool,
    pub has_claude_md: bool,
    pub git_branch: Option<String>,
    pub child_count: u32,
    pub descendant_count: u32,
    pub own_sessions: u32,
    pub subtree_sessions: u32,
    pub last_activity_ms: Option<i64>,
    pub own_score: f64,
    pub subtree_score: f64,
    pub top_level: String,
    /// Região definida para ESTA pasta em `config.projects` (a herança é do front).
    pub own_region: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeResult {
    pub nodes: Vec<TreeNode>,
    pub root_ids: Vec<String>,
    pub warnings: Vec<String>,
    pub truncated: bool,
}

struct Raw {
    path: PathBuf,
    parent: Option<usize>,
    depth: u32,
    name: String,
}

pub fn scan_tree(config: &Config, claude_projects_dir: &Path, now_ms: i64) -> Result<TreeResult, TreeError> {
    scan_tree_with(config, claude_projects_dir, now_ms, sessions::cache_file().as_deref(), DEFAULT_MAX_NODES)
}

/// Como `scan_tree`, com cache explícito (`None` = sem cache em disco) e teto de nós.
pub fn scan_tree_with(
    config: &Config,
    claude_projects_dir: &Path,
    now_ms: i64,
    cache_path: Option<&Path>,
    max_nodes: usize,
) -> Result<TreeResult, TreeError> {
    let mut warnings = Vec::new();
    let roots = usable_roots(config, &mut warnings);
    if roots.is_empty() {
        return Err(TreeError::NoRoots);
    }

    let hard_limit = max_nodes.saturating_mul(HARD_LIMIT_FACTOR);
    let mut raws: Vec<Raw> = Vec::new();
    let mut truncated = false;
    for root in &roots {
        if walk_root(root, config, hard_limit, &mut raws, &mut warnings) {
            truncated = true;
            break;
        }
    }
    if raws.len() > max_nodes {
        truncated = true;
    }
    if truncated {
        raws = cut_deepest_levels(raws, max_nodes);
        warnings.push(format!("árvore truncada: limite de {max_nodes} pastas (níveis mais profundos omitidos)"));
    }

    let n = raws.len();
    let paths: Vec<PathBuf> = raws.iter().map(|r| r.path.clone()).collect();
    let ids: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();

    // Sessões: só para pastas cuja codificação existe em ~/.claude/projects.
    let existing: HashSet<String> = fs::read_dir(claude_projects_dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    // Um único load/save do cache para toda a varredura.
    let mut cache = SessionIndexCache::load(cache_path);
    let mut own_sessions = vec![0u32; n];
    let mut last = vec![None::<i64>; n];
    let mut own_score = vec![0f64; n];
    let scope = ListScope { known: &paths, aggregate_children: false };
    for i in 0..n {
        if !existing.contains(&sessions::encode_project_dir(&paths[i])) {
            continue;
        }
        match sessions::list_sessions_scoped(&mut cache, claude_projects_dir, &paths[i], scope) {
            Ok(r) => {
                own_sessions[i] = u32::try_from(r.sessions.len()).unwrap_or(u32::MAX);
                for s in &r.sessions {
                    let at = s.last_timestamp_ms.unwrap_or(s.file_modified_ms);
                    last[i] = Some(last[i].map_or(at, |v: i64| v.max(at)));
                    own_score[i] += session_weight(now_ms, at, s.message_count);
                }
            }
            Err(e) => warnings.push(format!("sessões de {}: {e}", ids[i])),
        }
    }
    cache.save(cache_path);

    // Agregação de baixo para cima (pré-ordem invertida: filhos antes dos pais).
    let mut child_count = vec![0u32; n];
    let mut descendants = vec![0u32; n];
    let mut sub_sessions = own_sessions.clone();
    let mut sub_score = own_score.clone();
    let mut sub_last = last.clone();
    for i in (0..n).rev() {
        if let Some(p) = raws[i].parent {
            child_count[p] = child_count[p].saturating_add(1);
            descendants[p] = descendants[p].saturating_add(descendants[i]).saturating_add(1);
            sub_sessions[p] = sub_sessions[p].saturating_add(sub_sessions[i]);
            sub_score[p] += sub_score[i];
            sub_last[p] = match (sub_last[p], sub_last[i]) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        }
    }

    let regions = own_regions(config);
    let mut top_level = vec![String::new(); n];
    for i in 0..n {
        top_level[i] = match (raws[i].depth, raws[i].parent) {
            (0, _) | (_, None) => String::new(),
            (1, _) => raws[i].name.clone(),
            (_, Some(p)) => top_level[p].clone(),
        };
    }

    let nodes: Vec<TreeNode> = (0..n)
        .map(|i| {
            let has_git = paths[i].join(".git").exists();
            let has_claude_md = paths[i].join("CLAUDE.md").is_file();
            TreeNode {
                id: ids[i].clone(),
                path: ids[i].clone(),
                parent: raws[i].parent.map(|p| ids[p].clone()),
                name: raws[i].name.clone(),
                depth: raws[i].depth,
                is_project: has_git || has_claude_md,
                has_git,
                has_claude_md,
                git_branch: if has_git { git::read_branch(&paths[i]).map(|h| h.label()) } else { None },
                child_count: child_count[i],
                descendant_count: descendants[i],
                own_sessions: own_sessions[i],
                subtree_sessions: sub_sessions[i],
                last_activity_ms: sub_last[i],
                own_score: own_score[i],
                subtree_score: sub_score[i],
                top_level: top_level[i].clone(),
                own_region: regions.get(&paths[i]).cloned(),
            }
        })
        .collect();
    let root_ids = nodes.iter().filter(|x| x.parent.is_none()).map(|x| x.id.clone()).collect();
    Ok(TreeResult { nodes, root_ids, warnings, truncated })
}

/// Região própria por pasta: chaves de `config.projects` normalizadas (canônico
/// se existir, senão lexical); região inexistente em `config.regions` é ignorada.
fn own_regions(config: &Config) -> std::collections::HashMap<PathBuf, String> {
    config
        .project_meta_map()
        .into_iter()
        .filter_map(|(key, meta)| {
            let region = meta.region.as_ref().filter(|r| config.regions.iter().any(|x| &x.id == *r))?;
            Some((PathBuf::from(key), region.clone()))
        })
        .collect()
}

/// Raízes existentes e canônicas; duplicadas e aninhadas (dentro de outra raiz)
/// são removidas — a interna fica na árvore da externa.
fn usable_roots(config: &Config, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut canon: Vec<PathBuf> = Vec::new();
    for root in &config.roots {
        match fs::canonicalize(root) {
            Ok(c) if c.is_dir() => canon.push(c),
            Ok(_) => warnings.push(format!("raiz não é um diretório: {}", root.display())),
            Err(e) => warnings.push(format!("raiz inacessível ou inexistente: {} ({e})", root.display())),
        }
    }
    canon.sort();
    canon.dedup();
    let all = canon.clone();
    canon.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
    canon
}

/// Percorre uma raiz em pré-ordem (irmãos por nome, sem diferenciar caixa).
/// `true` = estourou o limite bruto.
fn walk_root(root: &Path, config: &Config, hard_limit: usize, raws: &mut Vec<Raw>, warnings: &mut Vec<String>) -> bool {
    let max_depth = usize::try_from(config.tree_max_depth).unwrap_or(usize::MAX);
    let mut it = WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(false)
        .sort_by(|a, b| {
            let (x, y) = (a.file_name().to_string_lossy(), b.file_name().to_string_lossy());
            x.to_lowercase().cmp(&y.to_lowercase()).then_with(|| x.cmp(&y))
        })
        .into_iter();
    let mut stack: Vec<usize> = Vec::new();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warnings.push(format!("não foi possível ler um diretório: {e}"));
                continue;
            }
        };
        // Só diretórios reais (symlinks não são seguidos nem viram nó).
        if !entry.file_type().is_dir() {
            continue;
        }
        let depth = entry.depth();
        if depth > 0 {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') || config.ignore.iter().any(|i| i.as_str() == name) {
                it.skip_current_dir();
                continue;
            }
        }
        if raws.len() >= hard_limit {
            return true;
        }
        stack.truncate(depth);
        let name = entry
            .path()
            .file_name()
            .map_or_else(|| entry.path().to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        raws.push(Raw {
            path: entry.path().to_path_buf(),
            parent: stack.last().copied(),
            depth: u32::try_from(depth).unwrap_or(u32::MAX),
            name,
        });
        stack.push(raws.len() - 1);
    }
    false
}

/// Mantém os níveis mais rasos que cabem em `max_nodes` (raízes sempre ficam).
fn cut_deepest_levels(raws: Vec<Raw>, max_nodes: usize) -> Vec<Raw> {
    let mut per_depth: BTreeMap<u32, usize> = BTreeMap::new();
    for r in &raws {
        *per_depth.entry(r.depth).or_default() += 1;
    }
    let mut keep_depth = 0u32;
    let mut total = 0usize;
    for (d, c) in per_depth {
        total += c;
        if total > max_nodes {
            break;
        }
        keep_depth = d;
    }
    let mut remap: Vec<Option<usize>> = vec![None; raws.len()];
    let mut out: Vec<Raw> = Vec::new();
    for (i, mut r) in raws.into_iter().enumerate() {
        if r.depth > keep_depth {
            continue;
        }
        r.parent = r.parent.and_then(|p| remap.get(p).copied().flatten());
        remap[i] = Some(out.len());
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Instant;

    struct Env {
        _td: tempfile::TempDir,
        root: PathBuf,
        cp: PathBuf,
    }

    fn env() -> Env {
        let td = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(td.path()).unwrap();
        let root = base.join("ws");
        let cp = base.join("claude");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&cp).unwrap();
        Env { _td: td, root, cp }
    }

    fn cfg(roots: &[&Path]) -> Config {
        Config { roots: roots.iter().map(|p| p.to_path_buf()).collect(), ..Config::default() }
    }

    fn run(c: &Config, cp: &Path) -> TreeResult {
        scan_tree_with(c, cp, 1_800_000_000_000, None, 5000).unwrap()
    }

    fn mkdir(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn session(cp: &Path, dir: &Path, n: u32, ts: &str) {
        let d = cp.join(sessions::encode_project_dir(dir));
        fs::create_dir_all(&d).unwrap();
        let line = format!(
            r#"{{"type":"user","timestamp":"{ts}","cwd":{},"message":{{"content":"oi"}}}}"#,
            serde_json::to_string(&dir.to_string_lossy()).unwrap()
        );
        fs::write(d.join(format!("00000000-0000-4000-8000-{n:012}.jsonl")), format!("{line}\n")).unwrap();
    }

    fn node<'a>(r: &'a TreeResult, name: &str) -> &'a TreeNode {
        r.nodes.iter().find(|n| n.name == name).unwrap()
    }

    #[test]
    fn estrutura_marcadores_ignorados_e_symlink() {
        let e = env();
        mkdir(&e.root, "a/b/c/d/e");
        mkdir(&e.root, "proj/.git");
        fs::write(e.root.join("proj/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        mkdir(&e.root, "proj/sub");
        mkdir(&e.root, "wt");
        fs::write(e.root.join("wt/.git"), "gitdir: x\n").unwrap();
        mkdir(&e.root, "a/b/CLAUDE_dir");
        fs::write(e.root.join("a/CLAUDE.md"), "x").unwrap();
        fs::write(e.root.join("CLAUDE.md"), "hub").unwrap();
        mkdir(&e.root, "node_modules/x");
        mkdir(&e.root, ".oculta/y");
        mkdir(&e.root, "alvo");
        symlink(e.root.join("alvo"), e.root.join("link")).unwrap();
        let r = run(&cfg(&[&e.root]), &e.cp);
        let names: Vec<_> = r.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(!names.contains(&"node_modules") && !names.contains(&".oculta") && !names.contains(&"link"));
        assert!(names.contains(&"sub") && names.contains(&"e"));
        let root = node(&r, "ws");
        assert!(root.is_project && root.has_claude_md && !root.has_git);
        assert_eq!(root.parent, None);
        assert_eq!(root.depth, 0);
        assert_eq!(root.top_level, "");
        let proj = node(&r, "proj");
        assert!(proj.has_git && proj.is_project);
        assert_eq!(proj.git_branch.as_deref(), Some("main"));
        assert_eq!(proj.top_level, "proj");
        assert_eq!(node(&r, "sub").top_level, "proj");
        assert_eq!(node(&r, "sub").parent.as_deref(), Some(proj.id.as_str()));
        assert!(node(&r, "wt").has_git);
        assert!(node(&r, "a").has_claude_md);
        assert_eq!(node(&r, "e").depth, 5);
        assert_eq!(node(&r, "e").top_level, "a");
        // projetos não interrompem a descida
        assert_eq!(proj.child_count, 1);
        assert_eq!(r.root_ids, vec![root.id.clone()]);
        assert!(!r.truncated);
    }

    #[test]
    fn ordem_pre_ordem_e_irmaos_sem_caixa_e_contagens() {
        let e = env();
        for d in ["b/z", "B2", "a/x", "A1", "c"] {
            mkdir(&e.root, d);
        }
        let r = run(&cfg(&[&e.root]), &e.cp);
        let names: Vec<_> = r.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["ws", "a", "x", "A1", "b", "z", "B2", "c"]);
        // pais antes dos filhos
        let pos: std::collections::HashMap<_, _> = r.nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
        for n in &r.nodes {
            if let Some(p) = &n.parent {
                assert!(pos[p] < pos[&n.id]);
            }
        }
        let root = node(&r, "ws");
        assert_eq!(root.child_count, 5);
        assert_eq!(root.descendant_count, 7);
        assert_eq!(node(&r, "a").child_count, 1);
        assert_eq!(node(&r, "c").descendant_count, 0);
    }

    #[test]
    fn profundidade_exata() {
        let e = env();
        mkdir(&e.root, "a/b/c/d");
        let mut c = cfg(&[&e.root]);
        c.tree_max_depth = 2;
        let r = run(&c, &e.cp);
        let names: Vec<_> = r.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["ws", "a", "b"]);
        assert_eq!(node(&r, "b").child_count, 0, "contagem reflete a árvore devolvida");
    }

    #[test]
    fn teto_de_nos_corta_niveis_mais_fundos() {
        let e = env();
        for i in 0..5 {
            mkdir(&e.root, &format!("p{i}/q{i}/r{i}"));
        }
        let r = scan_tree_with(&cfg(&[&e.root]), &e.cp, 0, None, 11).unwrap();
        assert!(r.truncated);
        assert!(r.warnings.iter().any(|w| w.contains("truncada")));
        // raiz + 5 + 5 = 11 cabem; o nível 3 não
        assert_eq!(r.nodes.len(), 11);
        assert!(r.nodes.iter().all(|n| n.depth <= 2));
        assert_eq!(node(&r, "q0").child_count, 0);
        let full = scan_tree_with(&cfg(&[&e.root]), &e.cp, 0, None, 1000).unwrap();
        assert!(!full.truncated && full.nodes.len() == 16);
    }

    #[test]
    fn sessoes_proprias_e_agregadas() {
        let e = env();
        let a = mkdir(&e.root, "a");
        let b = mkdir(&e.root, "a/b");
        let c = mkdir(&e.root, "c");
        session(&e.cp, &a, 1, "2026-10-01T00:00:00Z");
        session(&e.cp, &b, 2, "2026-10-05T00:00:00Z");
        session(&e.cp, &b, 3, "2026-10-03T00:00:00Z");
        let now = sessions::parse_iso_ms("2026-10-10T00:00:00Z").unwrap();
        let r = scan_tree_with(&cfg(&[&e.root]), &e.cp, now, None, 5000).unwrap();
        let (na, nb, nc, root) = (node(&r, "a"), node(&r, "b"), node(&r, "c"), node(&r, "ws"));
        assert_eq!((na.own_sessions, na.subtree_sessions), (1, 3));
        assert_eq!((nb.own_sessions, nb.subtree_sessions), (2, 2));
        assert_eq!((nc.own_sessions, nc.subtree_sessions), (0, 0));
        assert_eq!(root.subtree_sessions, 3);
        assert_eq!(na.last_activity_ms, sessions::parse_iso_ms("2026-10-05T00:00:00Z"));
        assert_eq!(nc.last_activity_ms, None);
        assert!(nb.own_score > 0.0 && na.subtree_score > na.own_score);
        assert!((root.subtree_score - (na.own_score + nb.own_score)).abs() < 1e-9);
        let _ = c;
    }

    #[test]
    fn colisao_de_codificacao_nao_duplica() {
        let e = env();
        let a = mkdir(&e.root, "b.c");
        let b = mkdir(&e.root, "b-c");
        session(&e.cp, &a, 1, "2026-10-01T00:00:00Z");
        session(&e.cp, &b, 2, "2026-10-01T00:00:00Z");
        let r = run(&cfg(&[&e.root]), &e.cp);
        assert_eq!(node(&r, "b.c").own_sessions, 1);
        assert_eq!(node(&r, "b-c").own_sessions, 1);
        assert_eq!(node(&r, "ws").subtree_sessions, 2);
    }

    #[test]
    fn varias_raizes_aninhada_dedupe_inexistente_e_sem_raizes() {
        let e = env();
        let outra = mkdir(e.root.parent().unwrap(), "outra");
        let inner = mkdir(&e.root, "x/inner");
        let c = cfg(&[&e.root, &outra, &inner, &e.root, Path::new("/nao/existe/zzz")]);
        let r = run(&c, &e.cp);
        assert_eq!(r.root_ids.len(), 2);
        assert_eq!(r.nodes.iter().filter(|n| n.name == "inner").count(), 1);
        assert_eq!(node(&r, "inner").depth, 2, "interna fica na árvore da externa");
        assert!(r.warnings.iter().any(|w| w.contains("inexistente")));
        let vazio = Config { roots: vec![PathBuf::from("/nao/existe/zzz")], ..Config::default() };
        assert!(matches!(scan_tree_with(&vazio, &e.cp, 0, None, 10), Err(TreeError::NoRoots)));
        let sem = Config { roots: vec![], ..Config::default() };
        assert!(matches!(scan_tree_with(&sem, &e.cp, 0, None, 10), Err(TreeError::NoRoots)));
    }

    #[test]
    fn serializacao_camel_case() {
        let e = env();
        let r = run(&cfg(&[&e.root]), &e.cp);
        let v = serde_json::to_value(&r).unwrap();
        let keys = |v: &serde_json::Value| -> std::collections::BTreeSet<String> { v.as_object().unwrap().keys().cloned().collect() };
        let set = |a: &[&str]| -> std::collections::BTreeSet<String> { a.iter().map(|s| s.to_string()).collect() };
        assert_eq!(keys(&v), set(&["nodes", "rootIds", "warnings", "truncated"]));
        assert_eq!(
            keys(&v["nodes"][0]),
            set(&[
                "id", "path", "parent", "name", "depth", "isProject", "hasGit", "hasClaudeMd", "gitBranch", "childCount",
                "descendantCount", "ownSessions", "subtreeSessions", "lastActivityMs", "ownScore", "subtreeScore", "topLevel", "ownRegion"
            ])
        );
    }

    #[test]
    fn own_region_valida_invalida_e_chave_nao_canonica() {
        use crate::config::ProjectMeta;
        let e = env();
        let a = mkdir(&e.root, "a");
        mkdir(&e.root, "b");
        mkdir(&e.root, "c");
        let meta = |r: &str| ProjectMeta { region: Some(r.into()), ..Default::default() };
        let mut c = cfg(&[&e.root]);
        // chave não canônica (com ./ e ..) e symlink resolvem para o id do nó
        symlink(&a, e.root.join("atalho")).unwrap();
        c.projects.insert(format!("{}/b/../b/./", e.root.display()), meta("diario"));
        c.projects.insert(e.root.join("atalho").to_string_lossy().into_owned(), meta("educacao"));
        c.projects.insert(e.root.join("c").to_string_lossy().into_owned(), meta("nao-existe"));
        let r = run(&c, &e.cp);
        assert_eq!(node(&r, "a").own_region.as_deref(), Some("educacao"));
        assert_eq!(node(&r, "b").own_region.as_deref(), Some("diario"));
        assert_eq!(node(&r, "c").own_region, None);
        assert_eq!(node(&r, "ws").own_region, None);
    }

    #[test]
    fn tres_mil_pastas_em_tempo_razoavel() {
        let e = env();
        for i in 0..30 {
            for j in 0..100 {
                fs::create_dir_all(e.root.join(format!("d{i}/s{j}"))).unwrap();
            }
        }
        let t = Instant::now();
        let r = run(&cfg(&[&e.root]), &e.cp);
        assert_eq!(r.nodes.len(), 1 + 30 + 3000);
        assert!(!r.truncated);
        assert!(t.elapsed().as_secs() < 20);
    }
}
