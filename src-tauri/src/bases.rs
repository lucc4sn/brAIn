//! Resolução das bases de contexto aplicáveis a uma pasta (pura e testável).

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::config::{Access, Config, RegionKind, normalize_key};
use crate::fs_util::{clip_debug, expand_tilde};

/// Como a base casou com a pasta: por caminho (`appliesTo`) ou por categoria.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BaseVia {
    Path,
    Region,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBase {
    pub id: String,
    /// Caminho canônico.
    pub path: PathBuf,
    pub is_dir: bool,
    pub label: Option<String>,
    /// O `applies_to` que casou (o mais específico); para `via == Region`, o id da região.
    pub inherited_from: PathBuf,
    /// Acesso efetivo: o da região `source` do CAMINHO DA BASE; fora disso, `Read`.
    pub access: Access,
    pub via: BaseVia,
}

#[derive(Debug, Clone, Default)]
pub struct Resolution {
    pub bases: Vec<ResolvedBase>,
    pub skipped: Vec<SkippedBase>,
}

/// Base que não pôde ser (totalmente) aplicada, com o motivo.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkippedBase {
    pub id: String,
    pub path: String,
    pub reason: String,
}

/// Política para conceder um caminho (pasta ou arquivo) ao claude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantPolicy {
    Allowed,
    /// Ampla demais para conceder automaticamente: só vai no prompt.
    PromptOnly,
    Forbidden(String),
}

const FORBIDDEN_SYSTEM: [&str; 12] =
    ["/etc", "/root", "/proc", "/sys", "/dev", "/boot", "/usr", "/var", "/run", "/bin", "/sbin", "/lib"];
const FORBIDDEN_HOME: [&str; 14] = [
    ".ssh",
    ".gnupg",
    ".config",
    ".claude",
    ".aws",
    ".kube",
    ".local/share/keyrings",
    ".claude.json",
    ".netrc",
    ".git-credentials",
    ".docker",
    ".password-store",
    ".mozilla",
    ".thunderbird",
];
/// Qualquer caminho com este componente é proibido (cópia criptografada da chave de emergência).
const FORBIDDEN_COMPONENT: &str = "chave-emergencia";

/// Máximo de bases injetadas no argv/prompt (as mais específicas).
pub const MAX_CONTEXT_BASES: usize = 20;
const PROMPT_ONLY: [&str; 3] = ["/home", "/mnt", "/media"];

/// Pura; espera caminhos canônicos (comparação por componentes de `Path`).
pub fn grant_policy(dir: &Path, home: Option<&Path>) -> GrantPolicy {
    if dir == Path::new("/") {
        return GrantPolicy::Forbidden("a raiz do sistema (/)".into());
    }
    for f in FORBIDDEN_SYSTEM.iter().chain(["/lib64"].iter()) {
        if dir.starts_with(f) {
            return GrantPolicy::Forbidden(format!("diretório de sistema ({f})"));
        }
    }
    if dir.components().any(|c| c.as_os_str() == FORBIDDEN_COMPONENT) {
        return GrantPolicy::Forbidden(format!("diretório sensível ({FORBIDDEN_COMPONENT})"));
    }
    if let Some(home) = home {
        for f in FORBIDDEN_HOME {
            if dir.starts_with(home.join(f)) {
                return GrantPolicy::Forbidden(format!("diretório sensível (~/{f})"));
            }
        }
        // Pasta que CONTÉM algo proibido (ou o próprio home): nunca concedida, só no prompt.
        if dir == home || FORBIDDEN_HOME.iter().any(|f| home.join(f).starts_with(dir)) {
            return GrantPolicy::PromptOnly;
        }
    }
    if PROMPT_ONLY.iter().any(|p| dir == Path::new(p)) {
        return GrantPolicy::PromptOnly;
    }
    GrantPolicy::Allowed
}

/// Home canônico (ou o bruto, se não der para canonicalizar).
pub fn home_dir_canon() -> Option<PathBuf> {
    let h = dirs::home_dir()?;
    Some(fs::canonicalize(&h).unwrap_or(h))
}

/// Caracteres que têm significado em regras de permissão (glob/lista) ou
/// quebrariam o JSON/argv; um caminho com qualquer um deles não pode ser restringido.
const SPECIAL_PATH_CHARS: &[char] = &['*', '?', '[', ']', '{', '}', '(', ')', '!', '#', ',', '"', '\'', '`', '\\'];

/// `true` se o caminho tem caractere especial (ou controle/invisível) e, portanto,
/// não pode virar regra de permissão precisa.
pub fn has_special_path_char(path: &Path) -> bool {
    let Some(t) = path.to_str() else { return true };
    t.chars().any(|c| SPECIAL_PATH_CHARS.contains(&c) || crate::fs_util::is_unsafe_char(c))
        // espaço no início/fim de um componente quebra a regra de permissão
        || path.components().any(|c| matches!(c, Component::Normal(n) if n.to_str().is_none_or(|n| n != n.trim())))
}

/// Base cuja pasta contém o projeto: conceder (e restringir) travaria o próprio projeto.
pub const CONTAINS_PROJECT_REASON: &str = "contém o projeto";

pub const SPECIAL_CHAR_REASON: &str = "caminho com caractere especial; não é possível restringir o acesso";

type MetaMap<'a> = std::collections::HashMap<String, &'a crate::config::ProjectMeta>;

/// Núcleo de `region_of`: `dir` já canônico/normalizado e `meta` já construído.
fn region_lookup<'a>(config: &'a Config, meta: &MetaMap<'_>, dir: &Path) -> Option<&'a str> {
    dir.ancestors().find_map(|a| {
        let r = meta.get(a.to_string_lossy().as_ref())?.region.as_deref()?;
        config.regions.iter().find(|g| g.id == r).map(|g| g.id.as_str())
    })
}

/// Categoria efetiva de `dir`: sobe pelos ancestrais; o primeiro cuja chave
/// normalizada tem `region` existente em `config.regions` vence (herança).
pub fn region_of<'a>(config: &'a Config, dir: &Path) -> Option<&'a str> {
    let meta = config.project_meta_map();
    region_lookup(config, &meta, &PathBuf::from(normalize_key(&dir.to_string_lossy())))
}

/// Acesso efetivo do caminho (canônico) da base: o da categoria `source` onde ele está; senão `Read`.
fn effective_access(config: &Config, meta: &MetaMap<'_>, path: &Path) -> Access {
    region_lookup(config, meta, path)
        .and_then(|id| config.regions.iter().find(|g| g.id == id))
        .filter(|g| g.kind == RegionKind::Source)
        .map_or(Access::Read, |g| g.access)
}

/// Formato enviado ao front (caminhos como texto).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedBaseView {
    pub id: String,
    pub path: String,
    pub is_dir: bool,
    pub label: Option<String>,
    pub inherited_from: String,
    pub access: Access,
    pub via: BaseVia,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionView {
    pub bases: Vec<ResolvedBaseView>,
    pub skipped: Vec<SkippedBase>,
}

impl From<&Resolution> for ResolutionView {
    fn from(r: &Resolution) -> Self {
        ResolutionView {
            bases: r
                .bases
                .iter()
                .map(|b| ResolvedBaseView {
                    id: b.id.clone(),
                    path: b.path.to_string_lossy().into_owned(),
                    is_dir: b.is_dir,
                    label: b.label.clone(),
                    inherited_from: b.inherited_from.to_string_lossy().into_owned(),
                    access: b.access,
                    via: b.via,
                })
                .collect(),
            skipped: r.skipped.clone(),
        }
    }
}

/// Remove `.` e resolve `..` apenas lexicalmente.
pub fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Alvo existente → canônico (casa symlinks); inexistente → lexical.
fn target_path(raw: &str) -> PathBuf {
    let p = expand_tilde(raw);
    fs::canonicalize(&p).unwrap_or_else(|_| normalize_lexical(&p))
}

/// Bases aplicáveis a `project_dir`: o diretório é o alvo ou está sob ele
/// (comparação por componentes de `Path`, nunca por string), ou sua categoria
/// efetiva está em `appliesToRegions`. Ordem: casamento por caminho (alvo mais
/// específico primeiro), depois por categoria, depois id; dedupe por caminho canônico.
pub fn resolve_bases(config: &Config, project_dir: &Path) -> Resolution {
    resolve_bases_with_home(config, project_dir, home_dir_canon().as_deref())
}

pub fn resolve_bases_with_home(config: &Config, project_dir: &Path, home: Option<&Path>) -> Resolution {
    let dir = fs::canonicalize(project_dir).unwrap_or_else(|_| normalize_lexical(project_dir));
    let mut found: Vec<ResolvedBase> = Vec::new();
    let mut skipped = Vec::new();
    let meta = config.project_meta_map();
    let dir_region = region_lookup(config, &meta, &dir);
    for base in &config.bases {
        let by_path = base
            .applies_to
            .iter()
            .map(|t| target_path(t))
            .filter(|t| dir.starts_with(t))
            .max_by_key(|t| t.components().count());
        let (inherited_from, via) = match (by_path, dir_region) {
            (Some(t), _) => (t, BaseVia::Path),
            (None, Some(r)) if base.applies_to_regions.iter().any(|x| x == r) => (PathBuf::from(r), BaseVia::Region),
            _ => continue,
        };
        let raw = expand_tilde(&base.path);
        match fs::canonicalize(&raw) {
            Ok(path) => found.push(ResolvedBase {
                id: base.id.clone(),
                is_dir: path.is_dir(),
                access: effective_access(config, &meta, &path),
                path,
                label: base.label.clone(),
                inherited_from,
                via,
            }),
            Err(e) => skipped.push(SkippedBase {
                id: base.id.clone(),
                path: base.path.clone(),
                reason: format!("base {}: caminho indisponível: {e}", clip_debug(&base.id)),
            }),
        }
    }
    // Caminho antes de categoria; entre caminhos, o alvo mais profundo primeiro.
    let rank = |b: &ResolvedBase| match b.via {
        BaseVia::Path => b.inherited_from.components().count() + 1,
        BaseVia::Region => 0,
    };
    found.sort_by(|a, b| rank(b).cmp(&rank(a)).then_with(|| a.id.cmp(&b.id)));
    let mut seen = std::collections::HashSet::new();
    found.retain(|b| seen.insert(b.path.clone()));
    // Política de concessão: proibidas saem; amplas ficam (só no prompt) com nota.
    let mut bases = Vec::new();
    for b in found {
        let path = b.path.to_string_lossy().into_owned();
        if has_special_path_char(&b.path) {
            skipped.push(SkippedBase { id: b.id.clone(), path, reason: SPECIAL_CHAR_REASON.into() });
            continue;
        }
        if b.is_dir && dir.starts_with(&b.path) {
            skipped.push(SkippedBase { id: b.id.clone(), path, reason: format!("base {}: {CONTAINS_PROJECT_REASON}", clip_debug(&b.id)) });
            continue;
        }
        let policy = grant_policy(&b.path, home);
        if matches!(policy, GrantPolicy::Allowed | GrantPolicy::PromptOnly) && bases.len() >= MAX_CONTEXT_BASES {
            skipped.push(SkippedBase {
                id: b.id.clone(),
                path,
                reason: format!("base {}: limite de {MAX_CONTEXT_BASES} bases", clip_debug(&b.id)),
            });
            continue;
        }
        match policy {
            GrantPolicy::Allowed => bases.push(b),
            GrantPolicy::PromptOnly => {
                skipped.push(SkippedBase {
                    id: b.id.clone(),
                    path: path.clone(),
                    reason: format!("sem acesso automático (pasta ampla): {}", clip_debug(&path)),
                });
                bases.push(b);
            }
            GrantPolicy::Forbidden(why) => skipped.push(SkippedBase {
                id: b.id.clone(),
                path,
                reason: format!("base {}: concessão proibida ({why})", clip_debug(&b.id)),
            }),
        }
    }
    Resolution { bases, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ContextBase, ProjectMeta, Region};
    use std::os::unix::fs::symlink;

    fn cfg(bases: Vec<ContextBase>) -> Config {
        Config { bases, ..Config::default() }
    }

    fn base(id: &str, path: &Path, targets: &[&Path]) -> ContextBase {
        ContextBase {
            id: id.into(),
            path: path.to_string_lossy().into_owned(),
            label: None,
            applies_to: targets.iter().map(|t| t.to_string_lossy().into_owned()).collect(),
            applies_to_regions: Vec::new(),
        }
    }

    struct Env {
        _td: tempfile::TempDir,
        root: PathBuf,
    }

    fn env() -> Env {
        let td = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(td.path()).unwrap();
        Env { _td: td, root }
    }

    fn dir(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn heranca_por_componentes_e_alvo_exato() {
        let e = env();
        let docs = dir(&e.root, "a/docs");
        let docs_x = dir(&e.root, "a/docs-x");
        let sub = dir(&e.root, "a/docs/sub/mais");
        let base_dir = dir(&e.root, "bases");
        let c = cfg(vec![base("b", &base_dir, &[&docs])]);
        assert_eq!(resolve_bases(&c, &docs).bases.len(), 1, "alvo exato");
        assert_eq!(resolve_bases(&c, &sub).bases.len(), 1, "descendente");
        assert!(resolve_bases(&c, &docs_x).bases.is_empty(), "/a/docs não casa /a/docs-x");
        assert!(resolve_bases(&c, &dir(&e.root, "a")).bases.is_empty(), "pai não herda");
    }

    #[test]
    fn mais_especifico_vence_multiplos_alvos_e_ordem() {
        let e = env();
        let a = dir(&e.root, "a");
        let ab = dir(&e.root, "a/b");
        let abc = dir(&e.root, "a/b/c");
        let other = dir(&e.root, "other");
        let b1 = dir(&e.root, "bases/b1");
        let b2 = dir(&e.root, "bases/b2");
        let b3 = dir(&e.root, "bases/b3");
        let c = cfg(vec![
            base("zeta", &b1, &[&other, &a]),
            base("alfa", &b2, &[&ab, &a]),
            base("meio", &b3, &[&a]),
        ]);
        let r = resolve_bases(&c, &abc);
        let ids: Vec<_> = r.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["alfa", "meio", "zeta"]);
        assert_eq!(r.bases[0].inherited_from, ab, "o mais específico");
        assert_eq!(r.bases[2].inherited_from, a);
    }

    #[test]
    fn inexistente_vai_para_skipped_e_dedupe() {
        let e = env();
        let p = dir(&e.root, "p");
        let real = dir(&e.root, "real");
        let c = cfg(vec![
            base("sumiu", &e.root.join("nao-existe"), &[&p]),
            base("x", &real, &[&p]),
            base("y", &real, &[&p]),
            base("fora", &e.root.join("tambem-nao"), &[&e.root.join("outro")]),
        ]);
        let r = resolve_bases(&c, &p);
        assert_eq!(r.bases.len(), 1, "dedupe por caminho canônico");
        assert_eq!(r.bases[0].id, "x");
        assert_eq!(r.skipped.len(), 1, "só conta quem se aplica: {:?}", r.skipped);
        assert_eq!(r.skipped[0].id, "sumiu");
        assert!(r.skipped[0].reason.contains("indisponível"));
    }

    #[test]
    fn symlink_alvo_inexistente_profundo_e_arquivo_fora() {
        let e = env();
        let real = dir(&e.root, "real");
        let proj = dir(&e.root, "real/p");
        symlink(&real, e.root.join("atalho")).unwrap();
        let fora = e.root.join("fora");
        fs::create_dir_all(&fora).unwrap();
        let arquivo = fora.join("nota.md");
        fs::write(&arquivo, "x").unwrap();
        // alvo via symlink casa o diretório real
        let c = cfg(vec![base("s", &arquivo, &[&e.root.join("atalho")])]);
        let r = resolve_bases(&c, &proj);
        assert_eq!(r.bases.len(), 1);
        assert!(!r.bases[0].is_dir);
        assert_eq!(r.bases[0].path, arquivo);
        // alvo que ainda não existe casa lexicalmente (com .. normalizado)
        let futuro = e.root.join("futuro/../futuro/pasta");
        let c = cfg(vec![base("f", &arquivo, &[&futuro])]);
        assert_eq!(resolve_bases(&c, &e.root.join("futuro/pasta/x")).bases.len(), 1, "ambos inexistentes: casa lexicalmente");
        fs::create_dir_all(e.root.join("futuro/pasta/x")).unwrap();
        assert_eq!(resolve_bases(&c, &e.root.join("futuro/pasta/x")).bases.len(), 1);
        // pasta profunda (> 8 níveis)
        let fundo = dir(&e.root, "n1/n2/n3/n4/n5/n6/n7/n8/n9/n10/n11");
        let c = cfg(vec![base("d", &arquivo, &[&e.root.join("n1")])]);
        assert_eq!(resolve_bases(&c, &fundo).bases.len(), 1);
    }

    #[test]
    fn view_serializa_em_camel_case() {
        let r = Resolution {
            bases: vec![ResolvedBase { id: "i".into(), path: "/p".into(), is_dir: true, label: None, inherited_from: "/q".into(), access: Access::Read, via: BaseVia::Path }],
            skipped: vec![SkippedBase { id: "s".into(), path: "/s".into(), reason: "r".into() }],
        };
        let v = serde_json::to_value(ResolutionView::from(&r)).unwrap();
        let mut ks: Vec<_> = v["bases"][0].as_object().unwrap().keys().cloned().collect();
        ks.sort();
        assert_eq!(ks, vec!["access", "id", "inheritedFrom", "isDir", "label", "path", "via"]);
        assert_eq!(v["bases"][0]["access"], "read");
        assert_eq!(v["bases"][0]["via"], "path");
        let mut sk: Vec<_> = v["skipped"][0].as_object().unwrap().keys().cloned().collect();
        sk.sort();
        assert_eq!(sk, vec!["id", "path", "reason"]);
    }

    #[test]
    fn tabela_de_grant_policy() {
        let home = Path::new("/home/u");
        let h = Some(home);
        let p = |s: &str| grant_policy(Path::new(s), h);
        assert!(matches!(p("/"), GrantPolicy::Forbidden(_)));
        for f in ["/etc", "/etc/ssh", "/root", "/proc/1", "/sys", "/dev/shm", "/boot", "/usr/share", "/var/lib", "/run/user", "/bin", "/sbin", "/lib", "/lib64/x",
            "/home/u/.ssh", "/home/u/.ssh/keys/k", "/home/u/.gnupg", "/home/u/.config", "/home/u/.config/brain", "/home/u/.claude/projects", "/home/u/.aws", "/home/u/.kube",
            "/home/u/.local/share/keyrings"] {
            assert!(matches!(p(f), GrantPolicy::Forbidden(_)), "{f}");
        }
        for f in ["/home/u", "/home", "/mnt", "/media"] {
            assert_eq!(p(f), GrantPolicy::PromptOnly, "{f}");
        }
        for f in ["/home/u/docs", "/home/u/.local/share/outro", "/home/u/.sshx", "/tmp/x", "/mnt/disco", "/home/outro", "/etcetera", "/usr2"] {
            assert_eq!(p(f), GrantPolicy::Allowed, "{f}");
        }
        assert_eq!(grant_policy(Path::new("/home/u/.ssh"), None), GrantPolicy::Allowed);
    }

    #[test]
    fn resolve_aplica_politica_e_symlink_para_ssh() {
        let e = env();
        let home = dir(&e.root, "home/u");
        let ssh = dir(&e.root, "home/u/.ssh");
        let docs = dir(&e.root, "home/u/docs");
        let proj = dir(&e.root, "proj");
        let nota_home = home.join("nota.md");
        fs::write(&nota_home, "x").unwrap();
        let chave = ssh.join("id");
        fs::write(&chave, "x").unwrap();
        symlink(&ssh, e.root.join("atalho-ssh")).unwrap();
        let c = cfg(vec![
            base("docs", &docs, &[&proj]),
            base("home-dir", &home, &[&proj]),
            base("arq-home", &nota_home, &[&proj]),
            base("ssh", &ssh, &[&proj]),
            base("arq-ssh", &chave, &[&proj]),
            base("link", &e.root.join("atalho-ssh"), &[&proj]),
        ]);
        let r = resolve_bases_with_home(&c, &proj, Some(&home));
        let ids: Vec<_> = r.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["arq-home", "docs", "home-dir"], "PromptOnly e Allowed ficam");
        let mut sk: Vec<_> = r.skipped.iter().map(|s| s.id.as_str()).collect();
        sk.sort();
        assert_eq!(sk, vec!["arq-ssh", "home-dir", "link"], "ssh e link são o mesmo caminho canônico (dedupe); arquivo no home é permitido");
        assert!(r.skipped.iter().any(|s| s.id == "home-dir" && s.reason.starts_with("sem acesso automático (pasta ampla): ")));
    }

    fn region(id: &str, kind: crate::config::RegionKind, access: Access) -> Region {
        Region { id: id.into(), name: id.into(), kind, access, ..Region::default() }
    }

    fn meta(region: &str) -> ProjectMeta {
        ProjectMeta { region: Some(region.into()), ..Default::default() }
    }

    #[test]
    fn region_of_herda_do_ancestral_mais_proximo_e_ignora_invalida() {
        let e = env();
        let area = dir(&e.root, "area");
        let sub = dir(&e.root, "area/sub");
        let fundo = dir(&e.root, "area/sub/fundo");
        let mut c = Config::default();
        c.projects.insert(area.to_string_lossy().into_owned(), meta("clientes"));
        assert_eq!(region_of(&c, &fundo), Some("clientes"));
        c.projects.insert(sub.to_string_lossy().into_owned(), meta("educacao"));
        assert_eq!(region_of(&c, &fundo), Some("educacao"), "mais próximo vence");
        c.projects.insert(sub.to_string_lossy().into_owned(), meta("nao-existe"));
        assert_eq!(region_of(&c, &fundo), Some("clientes"), "região inexistente é ignorada");
        assert_eq!(region_of(&c, &dir(&e.root, "fora")), None);
    }

    #[test]
    fn casamento_por_categoria_via_acesso_e_prioridade_do_caminho() {
        use crate::config::RegionKind::{Consumer, Source};
        let e = env();
        let proj = dir(&e.root, "trabalho/proj");
        let docs = dir(&e.root, "docs");
        let diario = dir(&e.root, "diario");
        let nota = diario.join("nota.md");
        fs::write(&nota, "x").unwrap();
        let mut c = Config {
            regions: vec![region("trab", Consumer, Access::Read), region("doc", Source, Access::Read), region("diar", Source, Access::Append)],
            ..Config::default()
        };
        c.projects.insert(e.root.join("trabalho").to_string_lossy().into_owned(), meta("trab"));
        c.projects.insert(docs.to_string_lossy().into_owned(), meta("doc"));
        c.projects.insert(diario.to_string_lossy().into_owned(), meta("diar"));
        let mut por_regiao = base("por-regiao", &docs, &[]);
        por_regiao.applies_to_regions = vec!["trab".into()];
        let mut diario_b = base("diario", &nota, &[]);
        diario_b.applies_to_regions = vec!["trab".into()];
        let mut ambos = base("ambos", &diario, &[&proj]);
        ambos.applies_to_regions = vec!["trab".into()];
        c.bases = vec![por_regiao, diario_b, ambos];
        let r = resolve_bases(&c, &proj);
        let ids: Vec<_> = r.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["ambos", "diario", "por-regiao"], "caminho antes de categoria; depois id");
        assert_eq!(r.bases[0].via, BaseVia::Path, "casou pelos dois: path");
        assert_eq!(r.bases[0].inherited_from, proj);
        assert_eq!(r.bases[1].via, BaseVia::Region);
        assert_eq!(r.bases[1].inherited_from, PathBuf::from("trab"));
        assert_eq!(r.bases[1].access, Access::Append, "arquivo sob categoria source/append");
        assert_eq!(r.bases[2].access, Access::Read);
        assert!(resolve_bases(&c, &dir(&e.root, "outro")).bases.is_empty());
    }

    #[test]
    fn caminho_com_caractere_especial_vai_para_skipped() {
        let e = env();
        let proj = dir(&e.root, "p");
        let estranho = dir(&e.root, "docs [v2]");
        let ok = dir(&e.root, "docs bom");
        let c = cfg(vec![base("x", &estranho, &[&proj]), base("y", &ok, &[&proj])]);
        let r = resolve_bases(&c, &proj);
        assert_eq!(r.bases.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), vec!["y"]);
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].reason, SPECIAL_CHAR_REASON);
        for p in ["/a*b", "/a?", "/a{b}", "/a(b)", "/a!", "/a#", "/a,b", "/a\"b", "/a'b", "/a\nb"] {
            assert!(has_special_path_char(Path::new(p)), "{p}");
        }
        assert!(!has_special_path_char(Path::new("/home/u/Educação/a b.md")));
    }

    #[test]
    fn grant_policy_ancestral_de_proibido_e_novos_proibidos() {
        let h = Some(Path::new("/home/u"));
        for p in ["/home/u/.local", "/home/u/.local/share"] {
            assert_eq!(grant_policy(Path::new(p), h), GrantPolicy::PromptOnly, "{p}");
        }
        for p in ["/home/u/.claude.json", "/home/u/.netrc", "/home/u/.git-credentials", "/home/u/.docker/c", "/home/u/.password-store",
            "/home/u/.mozilla/x", "/home/u/.thunderbird", "/home/u/w/chave-emergencia", "/srv/chave-emergencia/a.gpg"] {
            assert!(matches!(grant_policy(Path::new(p), h), GrantPolicy::Forbidden(_)), "{p}");
        }
        assert_eq!(grant_policy(Path::new("/home/u/.local/share/outro"), h), GrantPolicy::Allowed);
        assert!(has_special_path_char(Path::new("/a/ b")));
        assert!(has_special_path_char(Path::new("/a /b")));
        assert!(has_special_path_char(Path::new("/a/b ")));
        assert!(!has_special_path_char(Path::new("/a/b c")));
    }

    #[test]
    fn resolve_aplica_limite_e_pula_base_que_contem_o_projeto() {
        let e = env();
        let proj = dir(&e.root, "p");
        let mut bases = Vec::new();
        for i in 0..22 {
            bases.push(base(&format!("b{i:02}"), &dir(&e.root, &format!("bases/b{i:02}")), &[&proj]));
        }
        bases.push(base("pai", &e.root, &[&proj]));
        let r = resolve_bases(&cfg(bases), &proj);
        assert_eq!(r.bases.len(), MAX_CONTEXT_BASES);
        let lim: Vec<_> = r.skipped.iter().filter(|s| s.reason.contains("limite de 20 bases")).collect();
        assert_eq!(lim.len(), 2, "{:?}", r.skipped);
        assert!(r.skipped.iter().any(|s| s.id == "pai" && s.reason.contains("contém o projeto")));
    }
}
