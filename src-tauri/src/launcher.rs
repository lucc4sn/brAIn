//! Abre o Claude Code num terminal externo. Nunca usa shell: sempre
//! `Command::new(bin).args([...])` com argv em lista.

use std::env;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bases::{
    CONTAINS_PROJECT_REASON, GrantPolicy, ResolvedBase, SPECIAL_CHAR_REASON, grant_policy, has_special_path_char, home_dir_canon, resolve_bases,
};
use crate::config::{Access, Config, TerminalConfig, TerminalKind};
use crate::fs_util::{clip_debug, expand_tilde, is_executable, is_unsafe_char};
use crate::sessions::is_valid_session_id;

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("diretório inválido: {0}")]
    InvalidDir(String),
    #[error("o diretório não pertence a um projeto conhecido: {0}")]
    UnknownProject(String),
    #[error("id de sessão inválido")]
    InvalidSessionId,
    #[error("executável do claude não encontrado: {0}")]
    ClaudeNotFound(String),
    #[error("nenhum terminal disponível (tentados: {0})")]
    NoTerminal(String),
    #[error("terminal customizado sem executável configurado")]
    CustomNotConfigured,
    #[error("os argumentos do terminal customizado precisam conter o elemento exato {{command}}")]
    CustomMissingCommand,
    #[error("falha ao iniciar {program}: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("{program} encerrou logo após iniciar ({status})")]
    ExitedEarly { program: String, status: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LaunchMode {
    New,
    Resume { session_id: String },
    Continue,
}

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub dir: PathBuf,
    pub mode: LaunchMode,
    pub context: ContextArgs,
}

pub use crate::bases::MAX_CONTEXT_BASES;
const MAX_PROMPT_LABEL_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptEntry {
    path: PathBuf,
    is_dir: bool,
    label: Option<String>,
    access: Access,
    /// `false` = só no prompt (pasta ampla): nada é concedido.
    granted: bool,
}

/// Regras de permissão (`--settings`), ordenadas e sem duplicatas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PermissionRules {
    allow: BTreeSet<String>,
    ask: BTreeSet<String>,
    deny: BTreeSet<String>,
}

impl PermissionRules {
    fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.ask.is_empty() && self.deny.is_empty()
    }

    /// JSON de `--settings`; `None` sem regras.
    fn settings_json(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut perms = serde_json::Map::new();
        for (key, set) in [("allow", &self.allow), ("ask", &self.ask), ("deny", &self.deny)] {
            if !set.is_empty() {
                perms.insert(key.into(), serde_json::json!(set));
            }
        }
        serde_json::to_string(&serde_json::json!({ "permissions": perms })).ok()
    }
}

/// Regra no formato do claude: caminho absoluto = `//` + caminho sem a barra inicial.
/// Espera caminho sem caractere especial (ver `has_special_path_char`).
fn rule(tool: &str, path: &Path, suffix: &str) -> String {
    let abs = path.to_string_lossy();
    format!("{tool}(//{}{suffix})", abs.trim_start_matches('/'))
}

/// Bases de contexto já resolvidas, prontas para virar argv do claude.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextArgs {
    add_dirs: Vec<PathBuf>,
    rules: PermissionRules,
    entries: Vec<PromptEntry>,
}

fn sanitize_label(l: &str) -> Option<String> {
    let cleaned: String = l.chars().map(|c| if is_unsafe_char(c) { ' ' } else { c }).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed.chars().take(MAX_PROMPT_LABEL_CHARS).collect();
    (!cut.is_empty()).then_some(cut)
}

impl ContextArgs {
    /// `bases` já vem ordenada (mais específica primeiro). Devolve também os
    /// motivos das bases não aplicadas (caractere especial, limite, concessão
    /// proibida).
    ///
    /// Concessão precisa: base-PASTA vira `--add-dir` (mais regra `Edit` em `deny`
    /// se `read`, em `ask` se `append`); base-ARQUIVO NÃO concede a pasta pai —
    /// vira `Read(arquivo)` em `allow` (e `Edit(arquivo)` em `deny`/`ask`).
    /// `grant_policy` avalia o próprio caminho da base: concessão proibida (ex.:
    /// `~/.ssh`) pula tudo e a base some do prompt; ampla (ex.: `$HOME`) não
    /// concede nada, mas a base continua no prompt (o claude pede permissão).
    pub fn from_bases(bases: &[ResolvedBase]) -> (ContextArgs, Vec<String>) {
        Self::from_bases_with_home(bases, home_dir_canon().as_deref())
    }

    pub fn from_bases_with_home(bases: &[ResolvedBase], home: Option<&Path>) -> (ContextArgs, Vec<String>) {
        Self::from_bases_for(bases, None, home)
    }

    /// Como `from_bases_with_home`, sabendo o diretório do projeto: base-pasta que
    /// o contém (ancestral ou igual) é pulada — o `deny Edit` travaria o próprio projeto.
    pub fn from_bases_for(
        bases: &[ResolvedBase],
        project: Option<&Path>,
        home: Option<&Path>,
    ) -> (ContextArgs, Vec<String>) {
        let mut skipped = Vec::new();
        // (base, concedida?) das que passam nos filtros.
        let mut eligible: Vec<(&ResolvedBase, bool)> = Vec::new();
        for b in bases {
            let id = clip_debug(&b.id);
            if eligible.len() >= MAX_CONTEXT_BASES {
                skipped.push(format!("base {id}: acima do limite de {MAX_CONTEXT_BASES} bases"));
                continue;
            }
            if has_special_path_char(&b.path) {
                skipped.push(format!("base {id}: {SPECIAL_CHAR_REASON}"));
                continue;
            }
            if b.is_dir && project.is_some_and(|p| p.starts_with(&b.path)) {
                skipped.push(format!("base {id}: {CONTAINS_PROJECT_REASON}"));
                continue;
            }
            match grant_policy(&b.path, home) {
                GrantPolicy::Forbidden(why) => skipped.push(format!("base {id}: concessão proibida ({why})")),
                GrantPolicy::PromptOnly => eligible.push((b, false)),
                GrantPolicy::Allowed => eligible.push((b, true)),
            }
        }
        let mut ctx = ContextArgs::default();
        for &(b, granted) in &eligible {
            // O `deny` de uma pasta `read` externa vence o `ask` interno: rebaixa para `read`.
            let nested_in_read = eligible.iter().any(|&(o, og)| {
                og && o.is_dir && o.access == Access::Read && o.path != b.path && b.path.starts_with(&o.path)
            });
            let access = if nested_in_read { Access::Read } else { b.access };
            if granted {
                ctx.grant(b, access);
            }
            ctx.entries.push(PromptEntry {
                path: b.path.clone(),
                is_dir: b.is_dir,
                label: b.label.as_deref().and_then(sanitize_label),
                access,
                granted,
            });
        }
        (ctx, skipped)
    }

    fn grant(&mut self, b: &ResolvedBase, access: Access) {
        let edit_rules = if b.is_dir {
            if !self.add_dirs.contains(&b.path) {
                self.add_dirs.push(b.path.clone());
            }
            rule("Edit", &b.path, "/**")
        } else {
            self.rules.allow.insert(rule("Read", &b.path, ""));
            rule("Edit", &b.path, "")
        };
        match access {
            Access::Read => self.rules.deny.insert(edit_rules),
            Access::Append => self.rules.ask.insert(edit_rules),
        };
    }

    /// Bases efetivamente concedidas (pastas amplas, só no prompt, não contam).
    pub fn applied(&self) -> u32 {
        u32::try_from(self.entries.iter().filter(|e| e.granted).count()).unwrap_or(u32::MAX)
    }

    /// Texto do `--append-system-prompt` (None sem bases).
    pub fn prompt(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Bases de contexto definidas no brAIn para este projeto. Em caso de dúvida, consulte-as antes de supor:",
        );
        for e in &self.entries {
            text.push_str(&format!("\n- `{}` ({})", e.path.display(), if e.is_dir { "pasta" } else { "arquivo" }));
            if let Some(l) = &e.label {
                text.push_str(&format!(" ({l})"));
            }
            text.push_str(match e.access {
                Access::Read => " [somente leitura — não modifique]",
                Access::Append => {
                    " [pode acrescentar novos arquivos/entradas; nunca apagar, mover nem sobrescrever sem permissão expressa do usuário]"
                }
            });
        }
        Some(text)
    }

    /// Flags a anexar ao claude: `--add-dir=<DIR>` (um por diretório, forma com
    /// `=` para o variádico não engolir argumentos), `--settings <json>` (se houver
    /// regras; JSON num único elemento de argv) e UM `--append-system-prompt`.
    fn flags(&self) -> Vec<OsString> {
        let mut out = Vec::new();
        for d in &self.add_dirs {
            let mut a = OsString::from("--add-dir=");
            a.push(d.as_os_str());
            out.push(a);
        }
        if let Some(json) = self.rules.settings_json() {
            out.push("--settings".into());
            out.push(json.into());
        }
        if let Some(p) = self.prompt() {
            out.push("--append-system-prompt".into());
            out.push(p.into());
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStatus {
    pub kind: TerminalKind,
    pub executable: Option<String>,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOutcome {
    pub terminal_used: TerminalKind,
    pub bases_applied: u32,
    pub bases_skipped: Vec<String>,
}

pub struct ExternalTerminal {
    cfg: TerminalConfig,
}

const PTYXIS_BIN: &str = "ptyxis";
const GNOME_TERMINAL_BIN: &str = "gnome-terminal";

/// Entradas vazias ou relativas do `$PATH` são ignoradas (resolveriam contra o
/// diretório atual, que no spawn é o do projeto).
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|c| is_executable(c))
}

/// `claude_path` (config) > `~/.local/bin/claude` > `$PATH`. Se `claude_path`
/// estiver preenchido mas inválido, é erro (não cai silenciosamente adiante).
pub fn resolve_claude(cfg: &TerminalConfig) -> Result<PathBuf, LaunchError> {
    if let Some(p) = cfg.claude_path.as_deref().filter(|p| !p.trim().is_empty()) {
        let p = expand_tilde(p.trim());
        return if p.is_absolute() && is_executable(&p) { Ok(p) } else { Err(LaunchError::ClaudeNotFound(p.display().to_string())) };
    }
    if let Some(home) = dirs::home_dir() {
        let local = home.join(".local").join("bin").join("claude");
        if is_executable(&local) {
            return Ok(local);
        }
    }
    find_in_path("claude").ok_or_else(|| LaunchError::ClaudeNotFound("claude".into()))
}

pub fn detect_terminals() -> Vec<TerminalStatus> {
    [(TerminalKind::Ptyxis, PTYXIS_BIN), (TerminalKind::GnomeTerminal, GNOME_TERMINAL_BIN)]
        .into_iter()
        .map(|(kind, bin)| {
            let exe = find_in_path(bin);
            TerminalStatus { kind, found: exe.is_some(), executable: exe.map(|p| p.display().to_string()) }
        })
        .collect()
}

fn claude_flags(mode: &LaunchMode) -> Result<Vec<OsString>, LaunchError> {
    Ok(match mode {
        LaunchMode::New => vec![],
        LaunchMode::Continue => vec!["--continue".into()],
        LaunchMode::Resume { session_id } => {
            if !is_valid_session_id(session_id) {
                return Err(LaunchError::InvalidSessionId);
            }
            vec!["--resume".into(), session_id.into()]
        }
    })
}

/// Pura: monta o argv sem executar nada. O diretório sempre vai após `=`
/// (ou em `{dir}`), e o claude após `--`, logo nada é lido como flag.
pub fn build_command(
    cfg: &TerminalConfig,
    claude_bin: &Path,
    dir: &Path,
    mode: &LaunchMode,
) -> Result<CommandSpec, LaunchError> {
    build_command_with(cfg, claude_bin, dir, mode, &ContextArgs::default())
}

/// Como `build_command`, anexando as bases de contexto ao argv do claude
/// (`claude [modo] --add-dir=... --settings <json> --append-system-prompt <texto>`).
pub fn build_command_with(
    cfg: &TerminalConfig,
    claude_bin: &Path,
    dir: &Path,
    mode: &LaunchMode,
    context: &ContextArgs,
) -> Result<CommandSpec, LaunchError> {
    let mut command: Vec<OsString> = vec![claude_bin.as_os_str().to_owned()];
    command.extend(claude_flags(mode)?);
    command.extend(context.flags());

    let workdir = |flag: &str| {
        let mut a = OsString::from(flag);
        a.push(dir.as_os_str());
        a
    };
    match cfg.kind {
        TerminalKind::Ptyxis => {
            let mut args = vec!["--new-window".into(), workdir("--working-directory="), "--".into()];
            args.extend(command);
            Ok(CommandSpec { program: PathBuf::from(PTYXIS_BIN), args })
        }
        TerminalKind::GnomeTerminal => {
            let mut args = vec![workdir("--working-directory="), "--".into()];
            args.extend(command);
            Ok(CommandSpec { program: PathBuf::from(GNOME_TERMINAL_BIN), args })
        }
        TerminalKind::Custom => {
            let exe = cfg
                .custom_executable
                .as_deref()
                .filter(|e| !e.trim().is_empty())
                .ok_or(LaunchError::CustomNotConfigured)?;
            if !cfg.custom_args.iter().any(|a| a == "{command}") {
                return Err(LaunchError::CustomMissingCommand);
            }
            let mut args = Vec::new();
            for a in &cfg.custom_args {
                if a == "{command}" {
                    args.extend(command.iter().cloned());
                } else {
                    let mut out = OsString::new();
                    for (i, part) in a.split("{dir}").enumerate() {
                        if i > 0 {
                            out.push(dir.as_os_str());
                        }
                        out.push(part);
                    }
                    args.push(out);
                }
            }
            Ok(CommandSpec { program: expand_tilde(exe.trim()), args })
        }
    }
}

/// Valida diretório (canônico, é dir, projeto conhecido) e session-id.
/// Devolve o diretório canônico.
pub fn validate_launch(dir: &Path, mode: &LaunchMode, known_projects: &[String]) -> Result<PathBuf, LaunchError> {
    let canon = fs::canonicalize(dir).map_err(|e| LaunchError::InvalidDir(format!("{}: {e}", dir.display())))?;
    if !canon.is_dir() {
        return Err(LaunchError::InvalidDir(format!("{} não é um diretório", dir.display())));
    }
    let known = known_projects
        .iter()
        .any(|k| Path::new(k) == canon || fs::canonicalize(k).is_ok_and(|c| c == canon));
    if !known {
        return Err(LaunchError::UnknownProject(dir.display().to_string()));
    }
    if let LaunchMode::Resume { session_id } = mode
        && !is_valid_session_id(session_id)
    {
        return Err(LaunchError::InvalidSessionId);
    }
    Ok(canon)
}

/// Quanto tempo esperar para detectar um terminal que morre ao iniciar.
const EARLY_EXIT_WINDOW: Duration = Duration::from_millis(400);
const EARLY_EXIT_POLL: Duration = Duration::from_millis(20);

/// Inicia sem esperar o término. Se o processo sair com status != 0 dentro de
/// ~400 ms é erro (o chamador tenta o próximo terminal); saída 0 ou ainda vivo
/// = sucesso (uma thread colhe o processo para não deixar zumbi).
fn spawn_detached(spec: &CommandSpec, dir: &Path) -> Result<(), LaunchError> {
    let program = spec.program.display().to_string();
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|source| LaunchError::Spawn { program: program.clone(), source })?;
    let deadline = Instant::now() + EARLY_EXIT_WINDOW;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(LaunchError::ExitedEarly { program, status: status.to_string() }),
            Ok(None) => std::thread::sleep(EARLY_EXIT_POLL),
            Err(_) => break,
        }
    }
    reap_in_background(child);
    Ok(())
}

fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

impl ExternalTerminal {
    pub fn new(cfg: TerminalConfig) -> Self {
        ExternalTerminal { cfg }
    }

    /// Preferido primeiro; depois Ptyxis → GnomeTerminal.
    fn chain(&self) -> Vec<TerminalKind> {
        let mut chain = vec![self.cfg.kind];
        for k in [TerminalKind::Ptyxis, TerminalKind::GnomeTerminal] {
            if !chain.contains(&k) {
                chain.push(k);
            }
        }
        chain
    }

    fn is_available(&self, kind: TerminalKind) -> bool {
        match kind {
            TerminalKind::Ptyxis => find_in_path(PTYXIS_BIN).is_some(),
            TerminalKind::GnomeTerminal => find_in_path(GNOME_TERMINAL_BIN).is_some(),
            // Executável custom: caminho absoluto (após expandir `~/`) e executável.
            TerminalKind::Custom => self.cfg.custom_executable.as_deref().map(str::trim).is_some_and(|e| {
                let p = expand_tilde(e);
                !e.is_empty() && p.is_absolute() && is_executable(&p)
            }),
        }
    }

    /// Lança e informa qual terminal foi usado (pode ser um fallback).
    pub fn launch_with_outcome(&self, req: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        let claude = resolve_claude(&self.cfg)?;
        let mut tried = Vec::new();
        let mut last_spawn_err = None;
        for kind in self.chain() {
            if !self.is_available(kind) {
                tried.push(format!("{kind:?}"));
                continue;
            }
            let cfg = TerminalConfig { kind, ..self.cfg.clone() };
            let spec = build_command_with(&cfg, &claude, &req.dir, &req.mode, &req.context)?;
            match spawn_detached(&spec, &req.dir) {
                Ok(()) => {
                    return Ok(LaunchOutcome {
                        terminal_used: kind,
                        bases_applied: req.context.applied(),
                        bases_skipped: Vec::new(),
                    });
                }
                Err(e) => last_spawn_err = Some(e),
            }
        }
        Err(last_spawn_err.unwrap_or_else(|| LaunchError::NoTerminal(tried.join(", "))))
    }
}

/// Fluxo usado pelo comando Tauri: valida, resolve as bases de contexto da
/// pasta e lança.
pub fn launch_project_with_bases(
    project_path: &Path,
    mode: LaunchMode,
    known_projects: &[String],
    config: &Config,
) -> Result<LaunchOutcome, LaunchError> {
    let dir = validate_launch(project_path, &mode, known_projects)?;
    let (context, skipped) = prepare_context(config, &dir);
    let mut outcome = ExternalTerminal::new(config.terminal.clone())
        .launch_with_outcome(&LaunchRequest { dir, mode, context })?;
    outcome.bases_skipped = skipped;
    Ok(outcome)
}

/// Resolve as bases da pasta e monta os argumentos; devolve também os motivos
/// (strings) das bases puladas ou sem acesso automático.
pub fn prepare_context(config: &Config, dir: &Path) -> (ContextArgs, Vec<String>) {
    let resolution = resolve_bases(config, dir);
    let (context, limit_skipped) = ContextArgs::from_bases_for(&resolution.bases, Some(dir), home_dir_canon().as_deref());
    let mut skipped: Vec<String> = resolution.skipped.into_iter().map(|s| s.reason).collect();
    skipped.extend(limit_skipped);
    (context, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bases::BaseVia;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn cfg(kind: TerminalKind) -> TerminalConfig {
        TerminalConfig { kind, ..Default::default() }
    }

    #[test]
    fn ptyxis_novo_resume_continue() {
        let c = cfg(TerminalKind::Ptyxis);
        let claude = Path::new("/h/.local/bin/claude");
        let dir = Path::new("/p/meu proj");
        let s = build_command(&c, claude, dir, &LaunchMode::New).unwrap();
        assert_eq!(s.program, PathBuf::from("ptyxis"));
        assert_eq!(s.args, os(&["--new-window", "--working-directory=/p/meu proj", "--", "/h/.local/bin/claude"]));
        let s = build_command(&c, claude, dir, &LaunchMode::Resume { session_id: ID.into() }).unwrap();
        assert_eq!(s.args[3..], os(&["/h/.local/bin/claude", "--resume", ID])[..]);
        let s = build_command(&c, claude, dir, &LaunchMode::Continue).unwrap();
        assert_eq!(s.args.last().unwrap(), "--continue");
    }

    #[test]
    fn gnome_terminal() {
        let s = build_command(&cfg(TerminalKind::GnomeTerminal), Path::new("/c"), Path::new("/d"), &LaunchMode::Continue).unwrap();
        assert_eq!(s.program, PathBuf::from("gnome-terminal"));
        assert_eq!(s.args, os(&["--working-directory=/d", "--", "/c", "--continue"]));
    }

    #[test]
    fn custom_expande_command_e_dir() {
        let c = TerminalConfig {
            kind: TerminalKind::Custom,
            custom_executable: Some("/usr/bin/meuterm".into()),
            custom_args: vec!["-d".into(), "{dir}".into(), "--title=x:{dir}:y".into(), "-e".into(), "{command}".into()],
            claude_path: None,
        };
        let s = build_command(&c, Path::new("/c"), Path::new("/d d"), &LaunchMode::Resume { session_id: ID.into() }).unwrap();
        assert_eq!(s.program, PathBuf::from("/usr/bin/meuterm"));
        assert_eq!(s.args, os(&["-d", "/d d", "--title=x:/d d:y", "-e", "/c", "--resume", ID]));
    }

    #[test]
    fn custom_invalido() {
        let mut c = cfg(TerminalKind::Custom);
        assert!(matches!(
            build_command(&c, Path::new("/c"), Path::new("/d"), &LaunchMode::New),
            Err(LaunchError::CustomNotConfigured)
        ));
        c.custom_executable = Some("t".into());
        c.custom_args = vec!["{dir}".into()];
        assert!(matches!(
            build_command(&c, Path::new("/c"), Path::new("/d"), &LaunchMode::New),
            Err(LaunchError::CustomMissingCommand)
        ));
    }

    #[test]
    fn session_id_invalido_nunca_vira_argumento() {
        for bad in ["--dangerously-skip-permissions", "", "../x", "00000000-0000-4000-8000-00000000000g"] {
            let m = LaunchMode::Resume { session_id: bad.into() };
            for kind in [TerminalKind::Ptyxis, TerminalKind::GnomeTerminal, TerminalKind::Custom] {
                let mut c = cfg(kind);
                c.custom_executable = Some("t".into());
                c.custom_args = vec!["{command}".into()];
                assert!(matches!(
                    build_command(&c, Path::new("/c"), Path::new("/d"), &m),
                    Err(LaunchError::InvalidSessionId)
                ));
            }
        }
    }

    #[test]
    fn validate_launch_regras() {
        let t = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(t.path()).unwrap();
        let known = vec![dir.to_string_lossy().into_owned()];
        let ok = validate_launch(&dir, &LaunchMode::New, &known).unwrap();
        assert_eq!(ok, dir);
        assert!(matches!(validate_launch(&dir, &LaunchMode::New, &[]), Err(LaunchError::UnknownProject(_))));
        assert!(matches!(
            validate_launch(&dir.join("nao-existe"), &LaunchMode::New, &known),
            Err(LaunchError::InvalidDir(_))
        ));
        let file = dir.join("f");
        fs::write(&file, "x").unwrap();
        assert!(matches!(validate_launch(&file, &LaunchMode::New, &known), Err(LaunchError::InvalidDir(_))));
        let bad = LaunchMode::Resume { session_id: "--x".into() };
        assert!(matches!(validate_launch(&dir, &bad, &known), Err(LaunchError::InvalidSessionId)));
        let good = LaunchMode::Resume { session_id: ID.into() };
        assert!(validate_launch(&dir, &good, &known).is_ok());
    }

    #[test]
    fn resolve_claude_com_caminho_configurado() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("claude");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        let mut c = TerminalConfig { claude_path: Some(bin.to_string_lossy().into_owned()), ..Default::default() };
        assert!(matches!(resolve_claude(&c), Err(LaunchError::ClaudeNotFound(_))), "sem bit de execução");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve_claude(&c).unwrap(), bin);
        c.claude_path = Some("/nao/existe/claude".into());
        assert!(resolve_claude(&c).is_err());
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn saida_imediata_com_erro_e_detectada() {
        let t = tempfile::tempdir().unwrap();
        let bad = script(t.path(), "bad", "#!/bin/sh\nexit 3\n");
        let spec = CommandSpec { program: bad, args: vec![] };
        assert!(matches!(spawn_detached(&spec, t.path()), Err(LaunchError::ExitedEarly { .. })));
        let ok0 = script(t.path(), "ok0", "#!/bin/sh\nexit 0\n");
        assert!(spawn_detached(&CommandSpec { program: ok0, args: vec![] }, t.path()).is_ok());
        let alive = script(t.path(), "alive", "#!/bin/sh\nsleep 1\n");
        assert!(spawn_detached(&CommandSpec { program: alive, args: vec![] }, t.path()).is_ok());
        let missing = CommandSpec { program: t.path().join("nao-existe"), args: vec![] };
        assert!(matches!(spawn_detached(&missing, t.path()), Err(LaunchError::Spawn { .. })));
    }

    #[test]
    fn claude_path_relativo_e_recusado() {
        let c = TerminalConfig { claude_path: Some("bin/claude".into()), ..Default::default() };
        assert!(resolve_claude(&c).is_err());
    }

    #[test]
    fn modo_serializa_em_camel_case() {
        let m: LaunchMode = serde_json::from_str(r#"{"kind":"resume","sessionId":"abc"}"#).unwrap();
        assert_eq!(m, LaunchMode::Resume { session_id: "abc".into() });
        let m: LaunchMode = serde_json::from_str(r#"{"kind":"continue"}"#).unwrap();
        assert_eq!(m, LaunchMode::Continue);
    }

    #[test]
    fn cadeia_de_fallback() {
        let t = ExternalTerminal::new(cfg(TerminalKind::GnomeTerminal));
        assert_eq!(t.chain(), vec![TerminalKind::GnomeTerminal, TerminalKind::Ptyxis]);
        let t = ExternalTerminal::new(cfg(TerminalKind::Custom));
        assert_eq!(t.chain(), vec![TerminalKind::Custom, TerminalKind::Ptyxis, TerminalKind::GnomeTerminal]);
    }

    fn rb_acc(id: &str, path: &str, is_dir: bool, label: Option<&str>, access: Access) -> ResolvedBase {
        ResolvedBase {
            id: id.into(),
            path: PathBuf::from(path),
            is_dir,
            label: label.map(String::from),
            inherited_from: PathBuf::from("/alvo"),
            access,
            via: BaseVia::Path,
        }
    }

    fn rb(id: &str, path: &str, is_dir: bool, label: Option<&str>) -> ResolvedBase {
        rb_acc(id, path, is_dir, label, Access::Read)
    }

    fn argv(ctx: &ContextArgs, mode: &LaunchMode) -> Vec<OsString> {
        build_command_with(&cfg(TerminalKind::Ptyxis), Path::new("/c"), Path::new("/d"), mode, ctx).unwrap().args
    }

    const HEAD: &str = "Bases de contexto definidas no brAIn para este projeto. Em caso de dúvida, consulte-as antes de supor:";
    const RO: &str = " [somente leitura — não modifique]";
    const APPEND: &str =
        " [pode acrescentar novos arquivos/entradas; nunca apagar, mover nem sobrescrever sem permissão expressa do usuário]";

    fn count_prefix(a: &[OsString], prefix: &str) -> usize {
        a.iter().filter(|x| x.to_string_lossy().starts_with(prefix)).count()
    }

    fn settings_of(a: &[OsString]) -> serde_json::Value {
        let pos = a.iter().position(|x| x == "--settings").unwrap();
        serde_json::from_str(&a[pos + 1].to_string_lossy()).unwrap()
    }

    #[test]
    fn contexto_zero_uma_e_varias_bases() {
        let (vazio, sk) = ContextArgs::from_bases(&[]);
        assert!(sk.is_empty() && vazio.prompt().is_none() && vazio.applied() == 0);
        assert_eq!(argv(&vazio, &LaunchMode::New), os(&["--new-window", "--working-directory=/d", "--", "/c"]));

        let (um, _) = ContextArgs::from_bases(&[rb("a", "/abs/docs", true, Some("Docs"))]);
        let a = argv(&um, &LaunchMode::New);
        assert_eq!(a[3..7], os(&["/c", "--add-dir=/abs/docs", "--settings", r#"{"permissions":{"deny":["Edit(//abs/docs/**)"]}}"#])[..]);
        assert_eq!(a[7], OsString::from("--append-system-prompt"));
        assert_eq!(a[8], OsString::from(format!("{HEAD}\n- `/abs/docs` (pasta) (Docs){RO}")));
        assert_eq!(a.len(), 9);

        let (varias, _) = ContextArgs::from_bases(&[rb("a", "/abs/docs", true, None), rb("b", "/abs/nota.md", false, None)]);
        let a = argv(&varias, &LaunchMode::New);
        assert_eq!(count_prefix(&a, "--add-dir="), 1, "arquivo NÃO concede o diretório pai: {a:?}");
        assert_eq!(a[4], OsString::from("--add-dir=/abs/docs"));
        let s = settings_of(&a);
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Read(//abs/nota.md)"]));
        assert_eq!(s["permissions"]["deny"], serde_json::json!(["Edit(//abs/docs/**)", "Edit(//abs/nota.md)"]));
        assert!(s["permissions"].get("ask").is_none());
        let prompt = a.last().unwrap().to_string_lossy().into_owned();
        assert!(prompt.starts_with('B') && prompt.ends_with(&format!("- `/abs/nota.md` (arquivo){RO}")));
        assert_eq!(varias.applied(), 2);
    }

    #[test]
    fn acesso_append_usa_ask_e_prompt_proprio() {
        let (c, _) = ContextArgs::from_bases(&[
            rb_acc("d", "/abs/diario", true, None, Access::Append),
            rb_acc("f", "/abs/reg.md", false, None, Access::Append),
        ]);
        let a = argv(&c, &LaunchMode::New);
        let s = settings_of(&a);
        assert_eq!(s["permissions"]["ask"], serde_json::json!(["Edit(//abs/diario/**)", "Edit(//abs/reg.md)"]));
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Read(//abs/reg.md)"]));
        assert!(s["permissions"].get("deny").is_none());
        let p = c.prompt().unwrap();
        assert_eq!(p.matches(APPEND).count(), 2);
        assert!(!p.contains("somente leitura"));
    }

    #[test]
    fn sem_regras_nao_emite_settings() {
        // PromptOnly não concede nada: sem --add-dir e sem --settings.
        let (c, _) = ContextArgs::from_bases_with_home(&[rb("h", "/home/u", true, None)], Some(Path::new("/home/u")));
        let a = argv(&c, &LaunchMode::New);
        assert_eq!(count_prefix(&a, "--add-dir="), 0);
        assert!(!a.contains(&OsString::from("--settings")));
        assert_eq!(c.applied(), 0, "PromptOnly não conta como aplicada");
        assert!(c.prompt().is_some());
    }

    #[test]
    fn contexto_dedupe_de_diretorios_e_regras_ordenadas() {
        let (c, _) = ContextArgs::from_bases(&[
            rb("a", "/abs/docs", true, None),
            rb("b", "/abs/docs", true, None),
            rb("c", "/abs/z.md", false, None),
            rb("d", "/abs/a.md", false, None),
        ]);
        let a = argv(&c, &LaunchMode::New);
        assert_eq!(count_prefix(&a, "--add-dir="), 1);
        assert_eq!(count_prefix(&a, "--settings"), 1, "um único --settings");
        let s = settings_of(&a);
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Read(//abs/a.md)", "Read(//abs/z.md)"]));
        assert_eq!(c.applied(), 4);
    }

    #[test]
    fn contexto_caminhos_hostis_chegam_intactos_como_um_argv() {
        for p in ["/a b/c", "/a;b|c&d", "/uni/ação/$HOME/日本", "/a/--flag"] {
            let (c, sk) = ContextArgs::from_bases(&[rb("x", p, true, None)]);
            assert!(sk.is_empty());
            let a = argv(&c, &LaunchMode::New);
            assert_eq!(a[4], OsString::from(format!("--add-dir={p}")));
            assert_eq!(a[5], OsString::from("--settings"));
            assert!(a[8].to_string_lossy().contains(&format!("`{p}`")));
            assert_eq!(a.len(), 9, "nenhum argv extra");
        }
    }

    #[test]
    fn caractere_especial_no_caminho_nao_concede_nada() {
        for p in ["/a/*", "/a/b?c", "/a/[x]", "/a/{x,y}", "/a/$(rm -rf x)/`id`", "/a/b!", "/a/#c", "/a/\"q\"", "/a/it's"] {
            let (c, sk) = ContextArgs::from_bases(&[rb("x", p, true, None)]);
            assert_eq!(c.applied(), 0, "{p}");
            assert_eq!(sk.len(), 1, "{p}");
            assert!(sk[0].contains("caractere especial"), "{p}");
            let a = argv(&c, &LaunchMode::New);
            assert_eq!(a.len(), 4, "{p}: {a:?}");
        }
    }

    #[test]
    fn contexto_rotulo_sanitizado_e_controle_no_caminho_pulado() {
        let (c, sk) = ContextArgs::from_bases(&[
            rb("a", "/ok", true, Some("linha1\nlinha2\u{0}\t  fim")),
            rb("b", "/mau\ncaminho", true, None),
            rb("c", "/mau2\u{7}", false, None),
        ]);
        assert_eq!(sk.len(), 2);
        let p = c.prompt().unwrap();
        assert!(p.contains("(linha1 linha2 fim)"), "{p}");
        assert_eq!(p.lines().count(), 2, "o rótulo não quebra linhas: {p}");
        let longo = "é".repeat(200);
        let (c, _) = ContextArgs::from_bases(&[rb("a", "/ok", true, Some(&longo))]);
        assert!(c.prompt().unwrap().contains(&format!("({})", "é".repeat(80))));
        let (c, _) = ContextArgs::from_bases(&[rb("a", "/ok", true, Some(" \n "))]);
        assert!(!c.prompt().unwrap().contains("()"));
    }

    #[test]
    fn contexto_limite_de_20_bases() {
        let bases: Vec<_> = (0..25).map(|i| rb(&format!("b{i:02}"), &format!("/abs/{i}"), true, None)).collect();
        let (c, sk) = ContextArgs::from_bases(&bases);
        assert_eq!(c.applied(), 20);
        assert_eq!(sk.len(), 5);
        assert!(sk[0].contains("b20"));
        assert_eq!(count_prefix(&argv(&c, &LaunchMode::New), "--add-dir="), 20);
    }

    #[test]
    fn contexto_ordem_com_resume_e_continue() {
        let (c, _) = ContextArgs::from_bases(&[rb("a", "/abs/docs", true, None)]);
        let a = argv(&c, &LaunchMode::Resume { session_id: ID.into() });
        assert_eq!(a[3..9], os(&["/c", "--resume", ID, "--add-dir=/abs/docs", "--settings", r#"{"permissions":{"deny":["Edit(//abs/docs/**)"]}}"#])[..]);
        assert_eq!(a[9], OsString::from("--append-system-prompt"));
        let a = argv(&c, &LaunchMode::Continue);
        assert_eq!(a[3..7], os(&["/c", "--continue", "--add-dir=/abs/docs", "--settings"])[..]);
        assert_eq!(a[8], OsString::from("--append-system-prompt"));
        // custom: {command} expande o argv completo
        let cc = TerminalConfig {
            kind: TerminalKind::Custom,
            custom_executable: Some("/usr/bin/t".into()),
            custom_args: vec!["-e".into(), "{command}".into()],
            claude_path: None,
        };
        let s = build_command_with(&cc, Path::new("/c"), Path::new("/d"), &LaunchMode::New, &c).unwrap();
        assert_eq!(s.args.len(), 7);
        assert_eq!(s.args[2], OsString::from("--add-dir=/abs/docs"));
        assert_eq!(s.args[3], OsString::from("--settings"));
    }

    #[test]
    fn outcome_serializa_em_camel_case() {
        let o = LaunchOutcome { terminal_used: TerminalKind::Ptyxis, bases_applied: 2, bases_skipped: vec!["x".into()] };
        let v = serde_json::to_value(o).unwrap();
        let mut ks: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        ks.sort();
        assert_eq!(ks, vec!["basesApplied", "basesSkipped", "terminalUsed"]);
    }

    #[test]
    fn politica_de_concessao_no_launch() {
        let home = Path::new("/home/u");
        let (c, sk) = ContextArgs::from_bases_with_home(
            &[
                rb("ok", "/home/u/docs", true, None),
                rb("amplo-dir", "/home/u", true, None),
                rb("arq-home", "/home/u/nota.md", false, None),
                rb("ssh", "/home/u/.ssh", true, None),
                rb("arq-ssh", "/home/u/.ssh/id", false, None),
                rb("copia", "/home/u/.claude_copy/agents", true, None),
                rb("original", "/home/u/.claude/agents/x.md", false, None),
                rb("bidi", "/tmp/a\u{202e}b", true, None),
            ],
            Some(home),
        );
        let a = argv(&c, &LaunchMode::New);
        let adds: Vec<_> = a.iter().filter(|x| x.to_string_lossy().starts_with("--add-dir=")).collect();
        assert_eq!(adds, vec![&OsString::from("--add-dir=/home/u/docs"), &OsString::from("--add-dir=/home/u/.claude_copy/agents")]);
        let s = settings_of(&a);
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Read(//home/u/nota.md)"]), "arquivo concede só ele mesmo");
        // PromptOnly continua no prompt; proibidas somem do prompt
        let p = c.prompt().unwrap();
        assert!(p.contains("`/home/u` (pasta)") && p.contains("`/home/u/nota.md` (arquivo)") && p.contains("`/home/u/docs`"));
        assert!(!p.contains(".ssh") && !p.contains("/.claude/") && !p.contains("202e"));
        assert_eq!(c.applied(), 3, "o PromptOnly (/home/u) fica no prompt mas não conta");
        assert_eq!(sk.len(), 4, "{sk:?}");
    }

    #[test]
    fn prepare_context_prompt_only_traz_nota_em_skipped() {
        let td = tempfile::tempdir().unwrap();
        let proj = fs::canonicalize(td.path()).unwrap();
        let c = Config {
            bases: vec![crate::config::ContextBase {
                id: "amplo".into(),
                path: "/home".into(),
                applies_to: vec![proj.to_string_lossy().into_owned()],
                ..Default::default()
            }],
            ..Config::default()
        };
        let (ctx, skipped) = prepare_context(&c, &proj);
        assert!(ctx.add_dirs.is_empty() && ctx.rules.is_empty());
        assert!(ctx.prompt().unwrap().contains("`/home` (pasta)"));
        assert_eq!(skipped, vec!["sem acesso automático (pasta ampla): /home".to_string()]);
    }

    #[test]
    fn base_que_contem_o_projeto_nao_gera_regra() {
        let bases = [rb("pai", "/abs", true, None), rb("igual", "/abs/proj", true, None), rb("irma", "/abs/outra", true, None)];
        let (c, sk) = ContextArgs::from_bases_for(&bases, Some(Path::new("/abs/proj")), None);
        assert_eq!(c.applied(), 1);
        assert_eq!(sk.len(), 2);
        assert!(sk.iter().all(|m| m.contains("contém o projeto")), "{sk:?}");
        let a = argv(&c, &LaunchMode::New);
        assert_eq!(count_prefix(&a, "--add-dir="), 1);
        assert!(!settings_of(&a).to_string().contains("//abs/**"));
        // base-arquivo nunca contém o projeto
        let (c, sk) = ContextArgs::from_bases_for(&[rb("f", "/abs/proj", false, None)], Some(Path::new("/abs/proj")), None);
        assert!(sk.is_empty() && c.applied() == 1);
    }

    #[test]
    fn append_aninhada_em_read_e_rebaixada() {
        let (c, _) = ContextArgs::from_bases(&[
            rb_acc("ext", "/abs/docs", true, None, Access::Read),
            rb_acc("int", "/abs/docs/diario", true, None, Access::Append),
            rb_acc("arq", "/abs/docs/reg.md", false, None, Access::Append),
            rb_acc("solta", "/abs/solta", true, None, Access::Append),
        ]);
        let a = argv(&c, &LaunchMode::New);
        let s = settings_of(&a);
        assert_eq!(s["permissions"]["ask"], serde_json::json!(["Edit(//abs/solta/**)"]));
        let p = c.prompt().unwrap();
        assert_eq!(p.matches(APPEND).count(), 1, "só a solta pode acrescentar: {p}");
        assert_eq!(p.matches(RO).count(), 3);
    }

    #[test]
    fn pasta_ampla_nao_conta_como_aplicada_e_chave_emergencia_e_proibida() {
        let home = Path::new("/home/u");
        let (c, sk) = ContextArgs::from_bases_with_home(
            &[rb("local", "/home/u/.local", true, None), rb("ce", "/home/u/x/chave-emergencia/k", false, None)],
            Some(home),
        );
        assert_eq!(c.applied(), 0);
        assert_eq!(sk.len(), 1);
        assert_eq!(count_prefix(&argv(&c, &LaunchMode::New), "--add-dir="), 0);
    }
}
