//! Configuração persistida em `<config_dir>/brain/config.json`.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bases::{GrantPolicy, grant_policy, home_dir_canon, normalize_lexical};
use crate::fs_util::{clip_debug, expand_tilde, is_executable, is_unsafe_char, write_atomic};

const CONFIG_FILE: &str = "config.json";
const MAX_DEPTH_RANGE: std::ops::RangeInclusive<u32> = 1..=12;
const TREE_DEPTH_RANGE: std::ops::RangeInclusive<u32> = 1..=16;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("diretório de configuração do sistema indisponível")]
    NoConfigDir,
    #[error("erro de E/S em {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("configuração inválida: {0}")]
    Invalid(String),
    #[error("falha ao serializar a configuração: {0}")]
    Serialize(#[from] serde_json::Error),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TerminalKind {
    #[default]
    Ptyxis,
    GnomeTerminal,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalConfig {
    pub kind: TerminalKind,
    pub custom_executable: Option<String>,
    pub custom_args: Vec<String>,
    pub claude_path: Option<String>,
}

fn has_command_placeholder(args: &[String]) -> bool {
    args.iter().any(|a| a == "{command}")
}

/// Caminho preenchido: absoluto (após expandir `~/`), existente e executável.
fn check_executable(label: &str, value: &str) -> Result<(), ConfigError> {
    let p = expand_tilde(value);
    if !p.is_absolute() {
        return Err(ConfigError::Invalid(format!("{label} deve ser um caminho absoluto: {}", clip_debug(value))));
    }
    if !is_executable(&p) {
        return Err(ConfigError::Invalid(format!("{label} não existe ou não é executável: {}", clip_debug(value))));
    }
    Ok(())
}

fn filled(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl TerminalConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(p) = filled(&self.claude_path) {
            check_executable("terminal.claudePath", p)?;
        }
        if let Some(p) = filled(&self.custom_executable) {
            check_executable("terminal.customExecutable", p)?;
        }
        if self.kind == TerminalKind::Custom {
            if filled(&self.custom_executable).is_none() {
                return Err(ConfigError::Invalid("terminal customizado exige customExecutable".into()));
            }
            if !has_command_placeholder(&self.custom_args) {
                return Err(ConfigError::Invalid("customArgs precisa conter o elemento exato {command}".into()));
            }
        }
        Ok(())
    }

    /// Na carga: valores inaceitáveis são descartados com aviso (nunca falha).
    fn sanitize(&mut self, warnings: &mut Vec<String>) {
        for (label, field) in [("claudePath", &mut self.claude_path), ("customExecutable", &mut self.custom_executable)] {
            if let Some(v) = filled(field)
                && !expand_tilde(v).is_absolute()
            {
                warnings.push(format!("terminal.{label} descartado (não é caminho absoluto): {}", clip_debug(v)));
                *field = None;
            }
        }
        if self.kind == TerminalKind::Custom
            && (filled(&self.custom_executable).is_none() || !has_command_placeholder(&self.custom_args))
        {
            warnings.push("terminal customizado incompleto; usando Ptyxis".into());
            self.kind = TerminalKind::Ptyxis;
        }
    }
}

/// Papel da categoria: `Consumer` agrupa projetos; `Source` guarda material de
/// referência/registro que os projetos consultam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RegionKind {
    #[default]
    Consumer,
    Source,
}

/// Acesso do Claude ao material de uma categoria `Source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Read,
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub color: String,
    pub description: String,
    pub kind: RegionKind,
    /// Só importa para `kind == Source`.
    pub access: Access,
}

impl Default for Region {
    fn default() -> Self {
        Region {
            id: String::new(),
            name: String::new(),
            color: "#9AA0AB".into(),
            description: String::new(),
            kind: RegionKind::default(),
            access: Access::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectMeta {
    pub region: Option<String>,
    pub tags: Vec<String>,
    pub depends_on: Vec<String>,
    pub hidden: bool,
    pub display_name: Option<String>,
}

const MAX_BASES: usize = 200;
const MAX_APPLIES_TO: usize = 500;
const MAX_LABEL_CHARS: usize = 80;
const MAX_APPLIES_TO_REGIONS: usize = 16;

/// Base de contexto: arquivo ou pasta consultável por um conjunto de pastas-alvo
/// (herdada por todos os descendentes de cada alvo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextBase {
    pub id: String,
    /// Absoluto (aceita `~/`), arquivo ou pasta.
    pub path: String,
    pub label: Option<String>,
    /// Pastas absolutas; não precisam existir hoje.
    pub applies_to: Vec<String>,
    /// Ids de região: a base vale para toda pasta cuja região efetiva está aqui.
    pub applies_to_regions: Vec<String>,
}

fn has_unsafe(s: &str) -> bool {
    s.chars().any(is_unsafe_char)
}

fn check_abs_path(what: &str, s: &str) -> Result<(), String> {
    if has_unsafe(s) {
        return Err(format!("{what} contém caractere inseguro (controle/invisível)"));
    }
    if !expand_tilde(s).is_absolute() {
        return Err(format!("{what} deve ser um caminho absoluto: {}", clip_debug(s)));
    }
    Ok(())
}

/// Alvo (`appliesTo`): absoluto, seguro e sem `..`.
fn check_target(s: &str) -> Result<(), String> {
    check_abs_path("alvo (appliesTo)", s)?;
    if expand_tilde(s).components().any(|c| c == std::path::Component::ParentDir) {
        return Err(format!("alvo (appliesTo) não pode conter '..': {}", clip_debug(s)));
    }
    Ok(())
}

fn check_label(label: &str) -> Result<(), String> {
    if has_unsafe(label) || label.chars().count() > MAX_LABEL_CHARS {
        return Err(format!("rótulo inválido (sem caracteres inseguros, até {MAX_LABEL_CHARS} caracteres)"));
    }
    Ok(())
}

/// Chave canônica de `config.projects`: `~/` expandido, canônico se existir,
/// senão lexical (`.`/`..` resolvidos).
pub fn normalize_key(key: &str) -> String {
    let p = expand_tilde(key);
    fs::canonicalize(&p).unwrap_or_else(|_| normalize_lexical(&p)).to_string_lossy().into_owned()
}

impl ContextBase {
    /// O `path` NÃO precisa existir (base ausente vai para `skipped` no resolve),
    /// mas não pode estar sob um caminho proibido (comparação lexical se não existir).
    fn check(&self, region_ids: &HashSet<&str>) -> Result<(), String> {
        if self.id.trim().is_empty() || has_unsafe(&self.id) {
            return Err("id de base vazio ou com caractere inseguro".into());
        }
        check_abs_path("path da base", &self.path)?;
        if let GrantPolicy::Forbidden(why) = grant_policy(Path::new(&normalize_key(&self.path)), home_dir_canon().as_deref()) {
            return Err(format!("path da base proibido ({why}): {}", clip_debug(&self.path)));
        }
        if let Some(l) = &self.label {
            check_label(l)?;
        }
        if self.applies_to.len() > MAX_APPLIES_TO {
            return Err(format!("a base {} tem mais de {MAX_APPLIES_TO} alvos", clip_debug(&self.id)));
        }
        for t in &self.applies_to {
            check_target(t)?;
        }
        if self.applies_to.is_empty() && self.applies_to_regions.is_empty() {
            return Err(format!("a base {} precisa de ao menos um alvo (appliesTo ou appliesToRegions)", clip_debug(&self.id)));
        }
        if self.applies_to_regions.len() > MAX_APPLIES_TO_REGIONS {
            return Err(format!("a base {} tem mais de {MAX_APPLIES_TO_REGIONS} categorias", clip_debug(&self.id)));
        }
        let mut seen = HashSet::new();
        for r in &self.applies_to_regions {
            if has_unsafe(r) {
                return Err(format!("id de categoria contém caractere inseguro na base {}", clip_debug(&self.id)));
            }
            if !seen.insert(r.as_str()) {
                return Err(format!("categoria duplicada na base {}: {}", clip_debug(&self.id), clip_debug(r)));
            }
            if !region_ids.contains(r.as_str()) {
                return Err(format!("a base {} usa a categoria inexistente: {}", clip_debug(&self.id), clip_debug(r)));
            }
        }
        Ok(())
    }

    /// Na carga: descarta o que for inválido (alvos, categorias, rótulo).
    fn sanitize(&mut self, region_ids: &HashSet<&str>, warnings: &mut Vec<String>) {
        let id = clip_debug(&self.id);
        let mut seen = HashSet::new();
        self.applies_to_regions.retain(|r| {
            let ok = !has_unsafe(r) && region_ids.contains(r.as_str()) && seen.insert(r.clone());
            if !ok {
                warnings.push(format!("base {id}: categoria descartada (inexistente, duplicada ou inválida): {}", clip_debug(r)));
            }
            ok
        });
        if self.applies_to_regions.len() > MAX_APPLIES_TO_REGIONS {
            warnings.push(format!("base {id}: categorias acima de {MAX_APPLIES_TO_REGIONS} descartadas"));
            self.applies_to_regions.truncate(MAX_APPLIES_TO_REGIONS);
        }
        self.applies_to.retain(|t| {
            let ok = check_target(t).is_ok();
            if !ok {
                warnings.push(format!("base {id}: alvo descartado (inválido): {}", clip_debug(t)));
            }
            ok
        });
        if self.applies_to.len() > MAX_APPLIES_TO {
            warnings.push(format!("base {id}: alvos acima de {MAX_APPLIES_TO} descartados"));
            self.applies_to.truncate(MAX_APPLIES_TO);
        }
        if self.label.as_deref().is_some_and(|l| check_label(l).is_err()) {
            warnings.push(format!("base {id}: rótulo inválido descartado"));
            self.label = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub max_depth: u32,
    /// Profundidade máxima da varredura em árvore (`scan_tree`).
    pub tree_max_depth: u32,
    pub ignore: Vec<String>,
    pub show_orphans: bool,
    pub terminal: TerminalConfig,
    pub regions: Vec<Region>,
    pub projects: BTreeMap<String, ProjectMeta>,
    pub bases: Vec<ContextBase>,
}

fn default_regions() -> Vec<Region> {
    let r = |id: &str, name: &str, color: &str, description: &str, kind: RegionKind, access: Access| Region {
        id: id.into(),
        name: name.into(),
        color: color.into(),
        description: description.into(),
        kind,
        access,
    };
    use Access::{Append, Read};
    use RegionKind::{Consumer, Source};
    vec![
        r("projetos", "Projetos", "#8DA9C4", "Código e trabalhos em andamento.", Consumer, Read),
        r("educacao", "Educação", "#86B38A", "Estudos, cursos e laboratórios.", Consumer, Read),
        r("clientes", "Clientes", "#D98B6B", "Trabalho para terceiros.", Consumer, Read),
        r("documentacoes", "Documentações", "#D4B26A", "Referência canônica; o Claude só consulta.", Source, Read),
        r("agentes", "Agentes", "#6FB0A8", "Definições de subagentes; o Claude só consulta.", Source, Read),
        r(
            "diario",
            "Diário",
            "#BE8FA0",
            "Reports de incidente, aprendizados e registros; o Claude pode acrescentar, nunca apagar nem sobrescrever.",
            Source,
            Append,
        ),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: dirs::home_dir()
                .map(|h| vec![h.join("workspace")])
                .unwrap_or_default(),
            max_depth: 6,
            tree_max_depth: 8,
            ignore: [
                "node_modules",
                "target",
                ".venv",
                "venv",
                "dist",
                "build",
                "_arquivo",
                "chave-emergencia",
                ".cache",
                ".local",
                ".cargo",
                ".rustup",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            show_orphans: false,
            terminal: TerminalConfig::default(),
            regions: default_regions(),
            projects: BTreeMap::new(),
            bases: Vec::new(),
        }
    }
}

/// Resultado de `load`: `recovered` indica que o arquivo estava corrompido
/// (uma cópia foi guardada como `.bak`) e o padrão foi devolvido.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedConfig {
    pub config: Config,
    pub recovered: bool,
    /// Ajustes feitos na carga (raízes descartadas, caminhos inválidos...).
    pub warnings: Vec<String>,
}

impl LoadedConfig {
    fn clean(config: Config) -> Self {
        LoadedConfig { config, recovered: false, warnings: Vec::new() }
    }
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        if !MAX_DEPTH_RANGE.contains(&self.max_depth) {
            return invalid(format!("maxDepth deve estar entre 1 e 12 (recebido {})", self.max_depth));
        }
        if !TREE_DEPTH_RANGE.contains(&self.tree_max_depth) {
            return invalid(format!("treeMaxDepth deve estar entre 1 e 16 (recebido {})", self.tree_max_depth));
        }
        if self.regions.is_empty() {
            return invalid("é preciso ao menos uma região".into());
        }
        let mut seen = HashSet::new();
        for region in &self.regions {
            if region.id.trim().is_empty() {
                return invalid("id de região vazio".into());
            }
            if has_unsafe(&region.id) {
                return invalid("id de região com caractere inseguro".into());
            }
            if has_unsafe(&region.name) {
                return invalid(format!("nome da região {} com caractere inseguro", clip_debug(&region.id)));
            }
            if !seen.insert(region.id.as_str()) {
                return invalid(format!("id de região duplicado: {}", clip_debug(&region.id)));
            }
            if !is_hex_color(&region.color) {
                return invalid(format!("cor inválida na região {}: {} (use #RRGGBB)", clip_debug(&region.id), clip_debug(&region.color)));
            }
        }
        for (path, meta) in &self.projects {
            if let Some(r) = &meta.region
                && !seen.contains(r.as_str())
            {
                return invalid(format!("o projeto {} usa a região inexistente: {}", clip_debug(path), clip_debug(r)));
            }
        }
        if self.bases.len() > MAX_BASES {
            return invalid(format!("no máximo {MAX_BASES} bases de contexto"));
        }
        let mut ids = HashSet::new();
        for b in &self.bases {
            b.check(&seen).map_err(ConfigError::Invalid)?;
            if !ids.insert(b.id.as_str()) {
                return invalid(format!("id de base duplicado: {}", clip_debug(&b.id)));
            }
        }
        self.terminal.validate()
    }

    /// Ajusta valores fora do aceitável sem falhar (clampa profundidade,
    /// descarta raízes não absolutas e caminhos de terminal inválidos).
    fn sanitize(&mut self, warnings: &mut Vec<String>) {
        let clamped = self.max_depth.clamp(*MAX_DEPTH_RANGE.start(), *MAX_DEPTH_RANGE.end());
        if clamped != self.max_depth {
            warnings.push(format!("maxDepth {} ajustado para {clamped}", self.max_depth));
            self.max_depth = clamped;
        }
        let tree = self.tree_max_depth.clamp(*TREE_DEPTH_RANGE.start(), *TREE_DEPTH_RANGE.end());
        if tree != self.tree_max_depth {
            warnings.push(format!("treeMaxDepth {} ajustado para {tree}", self.tree_max_depth));
            self.tree_max_depth = tree;
        }
        self.roots.retain(|r| {
            let ok = r.is_absolute();
            if !ok {
                warnings.push(format!("raiz descartada (não é caminho absoluto): {}", clip_debug(&r.display().to_string())));
            }
            ok
        });
        self.terminal.sanitize(warnings);
        self.sanitize_regions(warnings);
        let mut ids = HashSet::new();
        let mut kept = Vec::new();
        let region_ids: HashSet<String> = self.regions.iter().map(|r| r.id.clone()).collect();
        let region_refs: HashSet<&str> = region_ids.iter().map(String::as_str).collect();
        for mut b in std::mem::take(&mut self.bases) {
            if kept.len() >= MAX_BASES {
                warnings.push(format!("base {} descartada: limite de {MAX_BASES} bases", clip_debug(&b.id)));
                continue;
            }
            b.sanitize(&region_refs, warnings);
            match b.check(&region_refs) {
                Ok(()) if ids.insert(b.id.clone()) => kept.push(b),
                Ok(()) => warnings.push(format!("base descartada: id duplicado {}", clip_debug(&b.id))),
                Err(e) => warnings.push(format!("base {} descartada: {}", clip_debug(&b.id), clip_debug(&e))),
            }
        }
        self.bases = kept;
        self.normalize_projects(warnings);
    }

    /// Regiões: id inseguro → região descartada; nome inseguro → caracteres trocados
    /// por espaço (vazio → usa o id). Projetos que apontam para região inexistente
    /// têm `region` zerada. Tudo com aviso.
    fn sanitize_regions(&mut self, warnings: &mut Vec<String>) {
        self.regions.retain(|r| {
            let ok = !has_unsafe(&r.id);
            if !ok {
                warnings.push(format!("região descartada (id com caractere inseguro): {}", clip_debug(&r.id)));
            }
            ok
        });
        for r in &mut self.regions {
            if has_unsafe(&r.name) {
                let cleaned: String = r.name.chars().map(|c| if is_unsafe_char(c) { ' ' } else { c }).collect();
                let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
                warnings.push(format!("região {}: nome com caractere inseguro corrigido", clip_debug(&r.id)));
                r.name = if cleaned.is_empty() { r.id.clone() } else { cleaned };
            }
        }
        let ids: HashSet<&str> = self.regions.iter().map(|r| r.id.as_str()).collect();
        for (path, meta) in &mut self.projects {
            if let Some(r) = &meta.region
                && !ids.contains(r.as_str())
            {
                warnings.push(format!("projeto {}: região inexistente {} removida", clip_debug(path), clip_debug(r)));
                meta.region = None;
            }
        }
    }

    /// Normaliza as chaves de `projects` (ver `normalize_key`). Colisões fundem os
    /// metadados: a chave que JÁ era igual à normalizada vence nos campos
    /// conflitantes (região, nome, hidden); sem ela, a menor em ordem alfabética.
    /// Região/nome ausentes no vencedor são preenchidos pelos demais; tags e
    /// dependsOn viram união ordenada sem duplicatas. Gera um warning por colisão.
    fn normalize_projects(&mut self, warnings: &mut Vec<String>) {
        let mut groups: BTreeMap<String, Vec<(String, ProjectMeta)>> = BTreeMap::new();
        for (k, m) in std::mem::take(&mut self.projects) {
            groups.entry(normalize_key(&k)).or_default().push((k, m));
        }
        for (norm, mut members) in groups {
            if let Some(pos) = members.iter().position(|(k, _)| *k == norm) {
                members.swap(0, pos);
                members[1..].sort_by(|a, b| a.0.cmp(&b.0));
            }
            if members.len() > 1 {
                warnings.push(format!(
                    "chaves de projects fundidas em {}: {}",
                    clip_debug(&norm),
                    members.iter().map(|(k, _)| clip_debug(k)).collect::<Vec<_>>().join(", ")
                ));
            }
            let mut it = members.into_iter();
            let Some((_, mut merged)) = it.next() else { continue };
            for (_, m) in it {
                merged.region = merged.region.or(m.region);
                merged.display_name = merged.display_name.or(m.display_name);
                merged.tags.extend(m.tags);
                merged.depends_on.extend(m.depends_on);
            }
            merged.tags.sort();
            merged.tags.dedup();
            merged.depends_on.sort();
            merged.depends_on.dedup();
            self.projects.insert(norm, merged);
        }
    }

    /// Cópia com as chaves de `projects` normalizadas (é o que vai para o disco).
    pub fn normalized(&self) -> Config {
        let mut c = self.clone();
        c.normalize_projects(&mut Vec::new());
        c
    }

    /// Metadados por chave normalizada — mesma função de normalização para
    /// quem consulta (árvore, descoberta).
    pub fn project_meta_map(&self) -> std::collections::HashMap<String, &ProjectMeta> {
        self.projects.iter().map(|(k, m)| (normalize_key(k), m)).collect()
    }

    /// Carrega de `<dir>/config.json`. Ausente → padrão (sem gravar).
    /// Corrompido → guarda um `.bak` (nunca sobrescreve, sem duplicar bytes
    /// idênticos), grava os padrões e devolve `recovered: true`.
    pub fn load_from(dir: &Path) -> Result<LoadedConfig, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadedConfig::clean(Config::default()));
            }
            Err(e) => return Err(io_err(&path)(e)),
        };
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let parsed = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .filter(serde_json::Value::is_object)
            .and_then(|v| serde_json::from_value::<Config>(v).ok());
        match parsed {
            Some(mut config) => {
                let mut warnings = Vec::new();
                config.sanitize(&mut warnings);
                Ok(LoadedConfig { config, recovered: false, warnings })
            }
            None => {
                backup_corrupt(dir, &bytes)?;
                let config = Config::default();
                let mut warnings = Vec::new();
                if let Err(e) = config.save_to(dir) {
                    warnings.push(format!("não foi possível regravar a configuração padrão: {e}"));
                }
                Ok(LoadedConfig { config, recovered: true, warnings })
            }
        }
    }

    /// Valida e grava de forma atômica (temporário único + `rename`, modo 0600).
    pub fn save_to(&self, dir: &Path) -> Result<(), ConfigError> {
        let normalized = self.normalized();
        normalized.validate()?;
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let json = serde_json::to_vec_pretty(&normalized)?;
        let target = dir.join(CONFIG_FILE);
        write_atomic(&target, &json, 0o600).map_err(io_err(&target))
    }

    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        dirs::config_dir().map(|d| d.join("brain")).ok_or(ConfigError::NoConfigDir)
    }

    pub fn load() -> Result<LoadedConfig, ConfigError> {
        Self::load_from(&Self::config_dir()?)
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::config_dir()?)
    }
}

/// Guarda o conteúdo corrompido em `config.json.bak`; se já existir, usa
/// `config.json.bak__<timestamp>` (e contador, se necessário). `create_new`
/// garante que nada é sobrescrito.
fn backup_corrupt(dir: &Path, bytes: &[u8]) -> Result<PathBuf, ConfigError> {
    // Já existe um backup com exatamente estes bytes? Então não cria outro.
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.filter_map(Result::ok) {
            let is_bak = e.file_name().to_string_lossy().starts_with("config.json.bak");
            if is_bak
                && e.metadata().is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64)
                && fs::read(e.path()).is_ok_and(|b| b == bytes)
            {
                return Ok(e.path());
            }
        }
    }
    let stamp = now_millis();
    let mut candidates = vec![dir.join(format!("{CONFIG_FILE}.bak"))];
    candidates.push(dir.join(format!("{CONFIG_FILE}.bak__{stamp}")));
    candidates.extend((1..100).map(|n| dir.join(format!("{CONFIG_FILE}.bak__{stamp}_{n}"))));
    for candidate in candidates {
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&candidate) {
            Ok(mut f) => {
                f.write_all(bytes).map_err(io_err(&candidate))?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&candidate)(e)),
        }
    }
    Err(ConfigError::Invalid("não foi possível escolher um nome de backup livre".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padrao_tem_seis_regioes_e_e_valido() {
        let c = Config::default();
        assert_eq!(c.regions.len(), 6);
        assert_eq!(c.regions[0].id, "projetos");
        let ids: Vec<_> = c.regions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["projetos", "educacao", "clientes", "documentacoes", "agentes", "diario"]);
        assert!(c.regions[..3].iter().all(|r| r.kind == RegionKind::Consumer));
        assert!(c.regions[3..].iter().all(|r| r.kind == RegionKind::Source));
        assert_eq!(c.regions[5].access, Access::Append);
        assert!(c.regions[3..5].iter().all(|r| r.access == Access::Read));
        assert_eq!(c.max_depth, 6);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ausente_devolve_padrao_sem_gravar() {
        let d = tempfile::tempdir().unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert!(!l.recovered);
        assert!(!d.path().join("config.json").exists());
    }

    #[test]
    fn roundtrip_atomico() {
        let d = tempfile::tempdir().unwrap();
        let mut c = Config { show_orphans: true, ..Config::default() };
        c.projects.insert("/p".into(), ProjectMeta { tags: vec!["a".into()], ..Default::default() });
        c.save_to(d.path()).unwrap();
        assert!(!d.path().join("config.json.tmp").exists());
        let l = Config::load_from(d.path()).unwrap();
        assert!(!l.recovered);
        assert_eq!(l.config, c);
    }

    #[test]
    fn campos_ausentes_usam_padrao() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("config.json"), r#"{"maxDepth": 3}"#).unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert_eq!(l.config.max_depth, 3);
        assert_eq!(l.config.regions.len(), 6);
    }

    #[test]
    fn corrompido_vai_para_bak_sem_sobrescrever() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.json");
        fs::write(&cfg, "{ quebrado").unwrap();
        fs::write(d.path().join("config.json.bak"), "BACKUP ANTIGO").unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert!(l.recovered);
        // os padrões foram regravados: a próxima carga é limpa
        assert!(!Config::load_from(d.path()).unwrap().recovered);
        assert_eq!(fs::read_to_string(d.path().join("config.json.bak")).unwrap(), "BACKUP ANTIGO");
        let novos: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.bak__"))
            .collect();
        assert_eq!(novos.len(), 1);
        assert_eq!(fs::read_to_string(novos[0].path()).unwrap(), "{ quebrado");
    }

    #[test]
    fn corrompido_primeira_vez_usa_bak_simples() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("config.json"), "nope").unwrap();
        assert!(Config::load_from(d.path()).unwrap().recovered);
        assert_eq!(fs::read_to_string(d.path().join("config.json.bak")).unwrap(), "nope");
    }

    #[test]
    fn validacao_rejeita_entradas_ruins() {
        let d = tempfile::tempdir().unwrap();
        let mut c = Config { max_depth: 0, ..Config::default() };
        assert!(matches!(c.save_to(d.path()), Err(ConfigError::Invalid(_))));
        c.max_depth = 13;
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.regions[0].color = "red".into();
        assert!(c.validate().is_err());
        c.regions[0].color = "#12345G".into();
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.regions[1].id = "projetos".into();
        assert!(c.validate().is_err());
        c.regions[1].id = "  ".into();
        assert!(c.validate().is_err());
        c.regions.clear();
        assert!(c.validate().is_err());
        assert!(!d.path().join("config.json").exists());
    }

    #[test]
    fn bom_array_e_dedupe_de_backup() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.json");
        fs::write(&cfg, "\u{feff}{\"maxDepth\": 4}").unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert!(!l.recovered);
        assert_eq!(l.config.max_depth, 4);
        fs::write(&cfg, "[]").unwrap();
        assert!(Config::load_from(d.path()).unwrap().recovered);
        fs::write(&cfg, "[]").unwrap();
        assert!(Config::load_from(d.path()).unwrap().recovered);
        let baks = fs::read_dir(d.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.bak"))
            .count();
        assert_eq!(baks, 1, "bytes idênticos não geram novo backup");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn sanitiza_na_carga() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("config.json"),
            r#"{"maxDepth": 99, "roots": ["rel/x", "/abs"], "terminal": {"claudePath": "claude"}}"#,
        )
        .unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert_eq!(l.config.max_depth, 12);
        assert_eq!(l.config.roots, vec![PathBuf::from("/abs")]);
        assert_eq!(l.config.terminal.claude_path, None);
        assert_eq!(l.warnings.len(), 3);
    }

    #[test]
    fn terminal_e_regiao_validados_no_save() {
        let d = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.terminal.claude_path = Some("claude".into());
        assert!(c.validate().is_err());
        c.terminal.claude_path = Some("/nao/existe/claude".into());
        assert!(c.validate().is_err());
        c.terminal = TerminalConfig { kind: TerminalKind::Custom, ..Default::default() };
        assert!(c.validate().is_err());
        c.terminal = TerminalConfig { kind: TerminalKind::Custom, custom_executable: Some("/bin/sh".into()), custom_args: vec!["x".into()], claude_path: None };
        assert!(c.validate().is_err());
        c.terminal.custom_args = vec!["{command}".into()];
        assert!(c.save_to(d.path()).is_ok());
        let mut c = Config::default();
        c.projects.insert("/p".into(), ProjectMeta { region: Some("nada".into()), ..Default::default() });
        assert!(c.validate().is_err());
    }

    fn base(id: &str, path: &str) -> ContextBase {
        ContextBase { id: id.into(), path: path.into(), applies_to: vec!["/proj".into()], ..Default::default() }
    }

    #[test]
    fn bases_validadas_no_save() {
        let d = tempfile::tempdir().unwrap();
        let existente = d.path().to_string_lossy().into_owned();
        let ok = Config { bases: vec![base("a", &existente)], ..Config::default() };
        assert!(ok.validate().is_ok());
        let try_with = |b: ContextBase| Config { bases: vec![b], ..Config::default() }.validate().is_err();
        assert!(try_with(base("", &existente)));
        assert!(try_with(base("a", "relativo/x")));
        // path inexistente é aceito (a ausência vai para `skipped` no resolve)
        assert!(!try_with(base("a", "/nao/existe/mesmo")));
        assert!(try_with(base("a", "/tmp/\u{0}x")));
        assert!(try_with(base("a", "/tmp/a\u{202e}b")));
        assert!(try_with(base("a", "/tmp/a\u{200b}b")));
        // caminhos proibidos (existentes ou não)
        for p in ["/", "/etc", "/etc/passwd", "/usr/share/x", "~/.ssh", "~/.ssh/id_rsa", "~/.config/brain", "~/.claude/projects"] {
            assert!(try_with(base("a", p)), "{p}");
        }
        assert!(!try_with(base("a", "~/docs/nota.md")));
        let mut b = base("a", &existente);
        b.applies_to = vec!["rel".into()];
        assert!(try_with(b));
        let mut b = base("a", &existente);
        b.applies_to = vec!["/ok".into(), "/x\ny".into()];
        assert!(try_with(b));
        let mut b = base("a", &existente);
        b.label = Some("x".repeat(81));
        assert!(try_with(b));
        let mut b = base("a", &existente);
        b.label = Some("a\nb".into());
        assert!(try_with(b));
        for l in ["a\u{202e}b", "x\u{200b}", "\u{feff}y"] {
            let mut b = base("a", &existente);
            b.label = Some(l.into());
            assert!(try_with(b), "{l:?}");
        }
        for t in ["/a/../b", "/x\u{2066}", "/y\u{200f}"] {
            let mut b = base("a", &existente);
            b.applies_to = vec![t.into()];
            assert!(try_with(b), "{t:?}");
        }
        let mut b = base("a", &existente);
        b.applies_to = (0..501).map(|i| format!("/p{i}")).collect();
        assert!(try_with(b));
        // alvo inexistente é permitido; id duplicado não
        let mut b = base("a", &existente);
        b.applies_to = vec!["/ainda/nao/existe".into()];
        assert!(Config { bases: vec![b.clone()], ..Config::default() }.validate().is_ok());
        assert!(Config { bases: vec![b.clone(), b], ..Config::default() }.validate().is_err());
        let muitas: Vec<_> = (0..201).map(|i| base(&format!("i{i}"), &existente)).collect();
        assert!(Config { bases: muitas, ..Config::default() }.validate().is_err());
    }

    #[test]
    fn bases_sanitizadas_na_carga_sem_erro() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("config.json"),
            r#"{"bases": [
              {"id": "ok", "path": "/nao/existe/ainda", "label": "bom", "appliesTo": ["/a", "rel", "/b\u0000"]},
              {"id": "ok", "path": "/x", "appliesTo": []},
              {"id": "", "path": "/x"},
              {"id": "rel", "path": "relativo"},
              {"id": "lab", "path": "/x", "label": "a\nb", "appliesTo": ["/l"]}
            ]}"#,
        )
        .unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert!(!l.recovered);
        let ids: Vec<_> = l.config.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["ok", "lab"]);
        assert_eq!(l.config.bases[0].applies_to, vec!["/a".to_string()]);
        assert_eq!(l.config.bases[1].label, None);
        assert!(l.warnings.len() >= 5);
    }

    #[test]
    fn bases_serializam_em_camel_case() {
        let c = Config { bases: vec![base("a", "/x")], ..Config::default() };
        let v = serde_json::to_value(&c).unwrap();
        let b = v["bases"][0].as_object().unwrap();
        let mut keys: Vec<_> = b.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["appliesTo", "appliesToRegions", "id", "label", "path"]);
    }

    #[test]
    fn base_ausente_pode_ser_salva_e_removida() {
        let d = tempfile::tempdir().unwrap();
        let c = Config { bases: vec![base("a", "/nao/existe/mesmo")], ..Config::default() };
        c.save_to(d.path()).unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert_eq!(l.config.bases.len(), 1);
        let sem = Config { bases: vec![], ..l.config };
        sem.save_to(d.path()).unwrap();
        assert!(Config::load_from(d.path()).unwrap().config.bases.is_empty());
    }

    #[test]
    fn carga_descarta_proibidas_inseguras_e_dotdot() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("config.json"),
            r#"{"bases": [
              {"id": "etc", "path": "/etc/passwd"},
              {"id": "ssh", "path": "~/.ssh/id_rsa"},
              {"id": "bidi", "path": "/tmp/a\u202eb"},
              {"id": "ok", "path": "/tmp/x", "label": "r\u200bx", "appliesTo": ["/a/../b", "/c", "/d\u2066"]}
            ]}"#,
        )
        .unwrap();
        let l = Config::load_from(d.path()).unwrap();
        let ids: Vec<_> = l.config.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["ok"]);
        assert_eq!(l.config.bases[0].applies_to, vec!["/c".to_string()]);
        assert_eq!(l.config.bases[0].label, None);
        assert!(l.warnings.len() >= 6, "{:?}", l.warnings);
    }

    #[test]
    fn chaves_de_projects_normalizadas_e_fundidas() {
        let d = tempfile::tempdir().unwrap();
        let home = dirs::home_dir().unwrap();
        let abs = home.join("brain-teste-inexistente/x");
        let meta = |r: &str, tags: &[&str], name: Option<&str>| ProjectMeta {
            region: Some(r.into()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            display_name: name.map(String::from),
            ..Default::default()
        };
        let mut c = Config::default();
        c.projects.insert("~/brain-teste-inexistente/x".into(), meta("projetos", &["b", "a"], Some("til")));
        c.projects.insert(abs.to_string_lossy().into_owned(), meta("educacao", &["c", "a"], None));
        c.projects.insert(format!("{}/./y/../x/", abs.parent().unwrap().display()), meta("diario", &[], Some("pt")));
        // sanitize (carga): a chave já normalizada vence nos conflitos; tags em união ordenada
        let mut w = Vec::new();
        c.normalize_projects(&mut w);
        assert_eq!(c.projects.len(), 1);
        let m = &c.projects[&abs.to_string_lossy().into_owned()];
        assert_eq!(m.region.as_deref(), Some("educacao"));
        assert_eq!(m.display_name.as_deref(), Some("pt"), "ausente no vencedor é preenchido pelo menor em ordem alfabética");
        assert_eq!(m.tags, vec!["a", "b", "c"]);
        assert_eq!(w.len(), 1);
        // save grava só a chave normalizada
        let mut c2 = Config::default();
        c2.projects.insert("~/brain-teste-inexistente/x".into(), meta("projetos", &[], None));
        c2.save_to(d.path()).unwrap();
        let l = Config::load_from(d.path()).unwrap();
        assert_eq!(l.config.projects.keys().collect::<Vec<_>>(), vec![&abs.to_string_lossy().into_owned()]);
    }

    #[test]
    fn regiao_serializa_kind_e_access_com_padrao() {
        let v = serde_json::to_value(&Config::default().regions[5]).unwrap();
        assert_eq!(v["kind"], "source");
        assert_eq!(v["access"], "append");
        let r: Region = serde_json::from_str(r##"{"id":"x","name":"X","color":"#112233","description":""}"##).unwrap();
        assert_eq!((r.kind, r.access), (RegionKind::Consumer, Access::Read));
    }

    #[test]
    fn bases_por_categoria_validadas() {
        let by_region = |regs: &[&str]| ContextBase {
            id: "r".into(),
            path: "/tmp/x".into(),
            applies_to_regions: regs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let valida = |b: ContextBase| Config { bases: vec![b], ..Config::default() }.validate().is_ok();
        assert!(valida(by_region(&["projetos", "diario"])));
        assert!(!valida(by_region(&[])), "sem nenhum alvo");
        assert!(!valida(by_region(&["nao-existe"])));
        assert!(!valida(by_region(&["projetos", "projetos"])));
        let mut mista = by_region(&["projetos"]);
        mista.applies_to = vec!["/a".into()];
        assert!(valida(mista));
        // limite de 16 (com ids existentes, para isolar a regra)
        let regions = (0..17).map(|i| Region { id: format!("r{i}"), ..Region::default() }).collect();
        let mut c = Config { regions, ..Config::default() };
        let ids: Vec<String> = c.regions.iter().map(|r| r.id.clone()).collect();
        c.bases = vec![ContextBase { id: "r".into(), path: "/tmp/x".into(), applies_to_regions: ids.clone(), ..Default::default() }];
        assert!(c.validate().is_err(), "17 categorias");
        c.bases[0].applies_to_regions = ids[..16].to_vec();
        assert!(c.validate().is_ok(), "16 categorias");
    }

    #[test]
    fn carga_descarta_categorias_inexistentes_e_base_sem_alvo() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("config.json"),
            r#"{"bases": [
              {"id": "a", "path": "/tmp/x", "appliesToRegions": ["diario", "fantasma", "diario"]},
              {"id": "b", "path": "/tmp/y", "appliesToRegions": ["fantasma"]},
              {"id": "c", "path": "/tmp/z", "appliesTo": ["/p"], "appliesToRegions": ["fantasma"]}
            ]}"#,
        )
        .unwrap();
        let l = Config::load_from(d.path()).unwrap();
        let ids: Vec<_> = l.config.bases.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);
        assert_eq!(l.config.bases[0].applies_to_regions, vec!["diario".to_string()]);
        assert!(l.config.bases[1].applies_to_regions.is_empty());
        assert!(l.warnings.len() >= 4, "{:?}", l.warnings);
    }

    #[test]
    fn carga_zera_regiao_de_projeto_orfa_e_trata_regiao_insegura() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("config.json"),
            r##"{"regions": [{"id":"ok","name":"Ok","color":"#112233"},
                            {"id":"ruim\u202e","name":"X","color":"#112233"},
                            {"id":"nome","name":"a\nb","color":"#112233"}],
               "projects": {"/p1": {"region":"frontal"}, "/p2": {"region":"ruim\u202e"}, "/p3": {"region":"ok"}}}"##,
        )
        .unwrap();
        let l = Config::load_from(d.path()).unwrap();
        let ids: Vec<_> = l.config.regions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["ok", "nome"]);
        assert_eq!(l.config.regions[1].name, "a b");
        assert_eq!(l.config.projects["/p1"].region, None);
        assert_eq!(l.config.projects["/p2"].region, None);
        assert_eq!(l.config.projects["/p3"].region.as_deref(), Some("ok"));
        assert!(l.warnings.len() >= 4);
        l.config.validate().unwrap();
        // validate rejeita id/nome inseguros
        let mut c = Config::default();
        c.regions[0].name = "a\u{202e}".into();
        assert!(c.validate().is_err());
    }
}
