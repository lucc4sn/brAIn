//! Leitura das sessões do Claude Code (`~/.claude/projects/<dir>/<uuid>.jsonl`).
//!
//! MÓDULO ISOLADO: o formato interno do Claude Code não é API estável. Todo o
//! conhecimento sobre ele fica aqui; campos ausentes ou desconhecidos nunca são
//! erro, no máximo marcam a sessão como `degraded`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::config::Config;
use crate::fs_util::write_atomic;

const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const TITLE_MAX_CHARS: usize = 80;
const ACTIVE_WINDOW_MS: i64 = 2 * 60 * 1000;
const CACHE_VERSION: u32 = 1;
const DAY_MS: f64 = 86_400_000.0;
const SCORE_WINDOW_DAYS: f64 = 30.0;
const SCORE_HALF_LIFE_DAYS: f64 = 14.0;
const UNTITLED: &str = "Sessão sem título";
const SYSTEM_PREFIXES: [&str; 7] = [
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command",
    "<system-reminder>",
    "<task-notification",
    "[Request interrupted",
];
const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("diretório home indisponível")]
    NoHome,
    #[error("erro de E/S em {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("nome de arquivo de sessão inválido: {0}")]
    InvalidFileName(String),
    #[error("sessão vazia ou sem nenhuma linha utilizável")]
    Empty,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SessionError + '_ {
    move |source| SessionError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TitleSource {
    AiTitle,
    LastPrompt,
    FirstPrompt,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub title_source: TitleSource,
    pub first_timestamp_ms: Option<i64>,
    pub last_timestamp_ms: Option<i64>,
    pub file_modified_ms: i64,
    /// Prompts humanos + respostas distintas.
    pub message_count: u32,
    /// Primeiro `cwd` visto.
    pub cwd: Option<String>,
    /// Último branch visto diferente de "HEAD".
    pub git_branch: Option<String>,
    pub degraded: bool,
    pub skipped_lines: u32,
    pub size_bytes: u64,
    /// Arquivo modificado há menos de 2 minutos.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadableSession {
    pub file_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsResult {
    pub sessions: Vec<SessionSummary>,
    pub degraded_count: u32,
    pub unreadable: Vec<UnreadableSession>,
}

// ---------------------------------------------------------------- utilitários

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub fn claude_projects_dir() -> Result<PathBuf, SessionError> {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("projects"))
        .ok_or(SessionError::NoHome)
}

pub fn cache_file() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("brain").join("sessions-index.json"))
}

/// Codificação (com perda) usada pelo Claude Code, que em JS substitui por
/// unidade UTF-16 (`/[^a-zA-Z0-9]/g`): cada unidade não alfanumérica ASCII vira
/// `-`; um caractere fora do BMP (emoji) ocupa duas unidades e vira `--`.
pub fn encode_project_dir(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    out
}

/// UUID 8-4-4-4-12 em hexadecimal (validação manual; impede `--flag`).
pub fn is_valid_session_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Exatamente dígitos ASCII (sem sinal), com tamanho dentro de `len`.
fn digits(s: &str, len: std::ops::RangeInclusive<usize>) -> Option<i64> {
    if !len.contains(&s.len()) || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Desloca `±HH:MM` / `±HHMM` para minutos (positivo = à frente de UTC).
fn parse_offset(tz: &str) -> Option<i64> {
    let sign = match tz.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let d = tz.get(1..)?;
    let (hh, mm) = match d.len() {
        5 if d.as_bytes().get(2) == Some(&b':') => (d.get(..2)?, d.get(3..)?),
        4 => (d.get(..2)?, d.get(2..)?),
        _ => return None,
    };
    let (hh, mm) = (digits(hh, 2..=2)?, digits(mm, 2..=2)?);
    (hh < 24 && mm < 60).then_some(sign * (hh * 60 + mm))
}

/// ISO-8601 (`Z` ou offset `±HH:MM`/`±HHMM`) → milissegundos UTC desde a época.
/// Ano 1..=9999, calendário validado (bissexto), só dígitos ASCII, aritmética
/// checada: entrada inválida é `None`, nunca pânico.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let (time, offset_min) = match rest.strip_suffix('Z') {
        Some(t) => (t, 0),
        None => {
            let (t, tz) = rest.split_at(rest.rfind(['+', '-'])?);
            (t, parse_offset(tz)?)
        }
    };
    let mut d = date.split('-');
    let year = digits(d.next()?, 4..=4)?;
    let month = digits(d.next()?, 2..=2)?;
    let day = digits(d.next()?, 2..=2)?;
    if d.next().is_some() || year < 1 || !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let (hms, frac) = match time.split_once('.') {
        Some((a, f)) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => (a, f),
        Some(_) => return None,
        None => (time, ""),
    };
    let mut t = hms.split(':');
    let h = digits(t.next()?, 2..=2)?;
    let mi = digits(t.next()?, 2..=2)?;
    let sec = digits(t.next()?, 2..=2)?;
    if t.next().is_some() || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let mut ms = 0i64;
    let mut n = 0;
    for b in frac.bytes().take(3) {
        ms = ms * 10 + i64::from(b - b'0');
        n += 1;
    }
    while n < 3 {
        ms *= 10;
        n += 1;
    }
    let secs = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(h * 3600 + mi * 60 + sec)?
        .checked_sub(offset_min * 60)?;
    secs.checked_mul(1000)?.checked_add(ms)
}

/// Valores já validados (ano ≤ 9999): não há risco de overflow aqui.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Colapsa whitespace e trunca a 80 caracteres (fronteira de char).
fn normalize_title(s: &str) -> String {
    let mut out = String::new();
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= TITLE_MAX_CHARS {
            break;
        }
    }
    out.chars().take(TITLE_MAX_CHARS).collect()
}

/// Texto injetado pelo sistema (não é prompt humano).
fn is_system_injection(text: &str) -> bool {
    let t = text.trim_start();
    SYSTEM_PREFIXES.iter().any(|p| t.starts_with(p))
}

// ------------------------------------------------------------ leitura de linhas

/// Lê uma linha (inclui o `\n`) sem nunca guardar mais de `max` bytes.
/// `None` = EOF; `Some(true)` = linha ok; `Some(false)` = linha longa demais
/// (descartada até o fim dela).
fn read_line_limited<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<Option<bool>> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(any.then_some(!too_long));
        }
        any = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        // O `\n` final não conta no limite.
        let content = if done { take - 1 } else { take };
        if !too_long {
            if buf.len() + content > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            return Ok(Some(!too_long));
        }
    }
}

// ------------------------------------------------------------- modelo bruto

/// Campo com tipo errado vira ausente em vez de descartar a linha inteira.
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        _ => None,
    })
}

fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => Some(b),
        _ => None,
    })
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    timestamp: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    git_branch: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool")]
    is_meta: Option<bool>,
    #[serde(default, deserialize_with = "lenient_bool")]
    is_sidechain: Option<bool>,
    #[serde(default, deserialize_with = "lenient_bool")]
    is_compact_summary: Option<bool>,
    #[serde(default, deserialize_with = "lenient_string")]
    request_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    uuid: Option<String>,
    message: Option<MessageField>,
    #[serde(default, deserialize_with = "lenient_string")]
    ai_title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    last_prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageField {
    Obj(RawMessage),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct RawMessage {
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<BlockField>),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BlockField {
    Obj(Block),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    text: Option<String>,
}

fn qualifies(t: &str) -> bool {
    !t.trim().is_empty() && !is_system_injection(t)
}

/// Texto do prompt humano, se a mensagem tiver algum: não vazio e que não seja
/// injeção do sistema (blocos só de `tool_result` não contam).
fn human_text(message: &Option<MessageField>) -> Option<&str> {
    let Some(MessageField::Obj(m)) = message else {
        return None;
    };
    match m.content.as_ref()? {
        Content::Text(s) => qualifies(s).then_some(s.as_str()),
        Content::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                BlockField::Obj(b) if b.kind.as_deref() == Some("text") => b.text.as_deref(),
                _ => None,
            })
            .find(|t| qualifies(t)),
        Content::Other(_) => None,
    }
}

fn non_blank(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

// ------------------------------------------------------------- acumulador

#[derive(Default)]
struct Accumulator {
    usable: u32,
    skipped: u32,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    cwd: Option<String>,
    branch: Option<String>,
    prompts: u32,
    first_prompt: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    responses: HashSet<String>,
    anonymous_responses: u32,
}

impl Accumulator {
    fn skip(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    fn feed(&mut self, bytes: &[u8]) {
        let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes).trim_ascii();
        if bytes.is_empty() {
            return;
        }
        if bytes.first() != Some(&b'{') {
            return self.skip();
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return self.skip();
        };
        let Ok(line) = serde_json::from_str::<RawLine>(text) else {
            return self.skip();
        };
        self.usable = self.usable.saturating_add(1);
        let kind = line.kind.clone().unwrap_or_default();
        match kind.as_str() {
            "ai-title" => {
                if let Some(t) = non_blank(line.ai_title) {
                    self.ai_title = Some(t);
                }
            }
            "last-prompt" => {
                if let Some(t) = non_blank(line.last_prompt) {
                    self.last_prompt = Some(t);
                }
            }
            "user" | "assistant" | "system" | "attachment" => self.feed_message(&kind, line),
            _ => {}
        }
    }

    fn feed_message(&mut self, kind: &str, line: RawLine) {
        if let Some(ms) = line.timestamp.as_deref().and_then(parse_iso_ms) {
            self.first_ts = Some(self.first_ts.map_or(ms, |v| v.min(ms)));
            self.last_ts = Some(self.last_ts.map_or(ms, |v| v.max(ms)));
        }
        if self.cwd.is_none() {
            self.cwd = non_blank(line.cwd.clone());
        }
        if let Some(b) = line.git_branch.as_deref()
            && !b.is_empty()
            && b != "HEAD"
        {
            self.branch = Some(b.to_string());
        }
        let sidechain = line.is_sidechain == Some(true);
        match kind {
            "user" if line.is_meta != Some(true) && !sidechain && line.is_compact_summary != Some(true) => {
                if let Some(text) = human_text(&line.message) {
                    self.prompts = self.prompts.saturating_add(1);
                    if self.first_prompt.is_none() {
                        let title = normalize_title(text);
                        if !title.is_empty() {
                            self.first_prompt = Some(title);
                        }
                    }
                }
            }
            "assistant" if !sidechain => match line.request_id.or(line.uuid) {
                Some(key) => {
                    self.responses.insert(key);
                }
                None => self.anonymous_responses = self.anonymous_responses.saturating_add(1),
            },
            _ => {}
        }
    }

    fn finish(self, id: String, size_bytes: u64, file_modified_ms: i64) -> Result<SessionSummary, SessionError> {
        if self.usable == 0 {
            return Err(SessionError::Empty);
        }
        let pick = |s: Option<String>| s.map(|t| normalize_title(&t)).filter(|t| !t.is_empty());
        let (title, title_source) = if let Some(t) = pick(self.ai_title) {
            (t, TitleSource::AiTitle)
        } else if let Some(t) = pick(self.last_prompt) {
            (t, TitleSource::LastPrompt)
        } else if let Some(t) = self.first_prompt {
            (t, TitleSource::FirstPrompt)
        } else {
            (UNTITLED.to_string(), TitleSource::None)
        };
        let responses = u32::try_from(self.responses.len()).unwrap_or(u32::MAX);
        Ok(SessionSummary {
            id,
            title,
            title_source,
            first_timestamp_ms: self.first_ts,
            last_timestamp_ms: self.last_ts,
            file_modified_ms,
            message_count: self
                .prompts
                .saturating_add(responses)
                .saturating_add(self.anonymous_responses),
            cwd: self.cwd,
            git_branch: self.branch,
            degraded: self.skipped > 0,
            skipped_lines: self.skipped,
            size_bytes,
            active: is_active(file_modified_ms),
        })
    }
}

fn is_active(file_modified_ms: i64) -> bool {
    now_ms().saturating_sub(file_modified_ms) < ACTIVE_WINDOW_MS
}

/// Id vem do nome do arquivo (não do `sessionId` interno).
fn session_id_from_path(path: &Path) -> Result<String, SessionError> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if is_valid_session_id(stem) {
        Ok(stem.to_string())
    } else {
        Err(SessionError::InvalidFileName(
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        ))
    }
}

pub fn parse_session_file(path: &Path) -> Result<SessionSummary, SessionError> {
    let id = session_id_from_path(path)?;
    let meta = fs::metadata(path).map_err(io_err(path))?;
    let file = File::open(path).map_err(io_err(path))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut acc = Accumulator::default();
    let mut buf = Vec::new();
    while let Some(ok) = read_line_limited(&mut reader, &mut buf, MAX_LINE_BYTES).map_err(io_err(path))? {
        if ok {
            acc.feed(&buf);
        } else {
            acc.skip();
        }
    }
    acc.finish(id, meta.len(), mtime_ms(&meta))
}

// ------------------------------------------------------------------ cache

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime_ms: i64,
    size: u64,
    summary: SessionSummary,
}

/// Índice em disco: chave = caminho; vale se (mtime_ms, size) coincidem.
/// Qualquer falha de leitura/escrita é ignorada (reparseia).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionIndexCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
    #[serde(skip)]
    dirty: bool,
}

impl SessionIndexCache {
    pub fn empty() -> Self {
        SessionIndexCache { version: CACHE_VERSION, ..Default::default() }
    }

    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::empty();
        };
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionIndexCache>(&b).ok())
            .filter(|c| c.version == CACHE_VERSION)
            .unwrap_or_else(Self::empty)
    }

    /// Grava de forma atômica se houve mudança; falhas são ignoradas.
    pub fn save(&mut self, path: Option<&Path>) {
        let Some(path) = path else { return };
        if !self.dirty {
            return;
        }
        if self.write_atomic(path).is_ok() {
            self.dirty = false;
        }
    }

    fn write_atomic(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec(self).map_err(io::Error::other)?;
        write_atomic(path, &json, 0o600)
    }

    /// Remove as entradas de arquivos desta pasta que não existem mais.
    fn prune_dir(&mut self, dir: &Path, seen: &HashSet<String>) {
        let prefix = format!("{}/", dir.to_string_lossy());
        let before = self.entries.len();
        self.entries.retain(|k, _| !k.starts_with(&prefix) || seen.contains(k));
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    fn get_or_parse(&mut self, path: &Path) -> Result<SessionSummary, SessionError> {
        let key = path.to_string_lossy().into_owned();
        if let Ok(meta) = fs::metadata(path)
            && let Some(e) = self.entries.get(&key)
            && e.mtime_ms == mtime_ms(&meta)
            && e.size == meta.len()
        {
            let mut summary = e.summary.clone();
            summary.active = is_active(summary.file_modified_ms);
            return Ok(summary);
        }
        let summary = parse_session_file(path)?;
        self.entries.insert(
            key,
            CacheEntry {
                mtime_ms: summary.file_modified_ms,
                size: summary.size_bytes,
                summary: summary.clone(),
            },
        );
        self.dirty = true;
        Ok(summary)
    }
}

// ----------------------------------------------------------------- listagem

/// Contexto para atribuir sessões a projetos.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListScope<'a> {
    /// Projetos conhecidos (do scan): só servem para detectar colisões de
    /// codificação e para não engolir sessões de projetos mais específicos.
    pub known: &'a [PathBuf],
    /// Agrega as sessões de subpastas (ex.: `proj/src`) que não são projeto.
    pub aggregate_children: bool,
}

fn path_matches(cwd: &Path, candidate: &Path, project: &Path, project_canon: Option<&Path>) -> bool {
    cwd.starts_with(candidate) || (candidate == project && project_canon.is_some_and(|c| cwd.starts_with(c)))
}

/// Regra de atribuição: a PASTA CODIFICADA é a regra principal — as sessões de
/// `encode_project_dir(P)` são de P, sem olhar o `cwd` (pastas renomeadas ou
/// reorganizadas gravam `cwd` antigo). O `cwd` só desempata quando mais de um
/// projeto conhecido codifica para a mesma pasta (`b.c` vs `b-c`): vence o
/// candidato mais específico que contém o `cwd`; sem correspondência (ou sem
/// `cwd`), o de caminho lexicograficamente menor — a sessão não é duplicada.
fn owns_session(cwd: Option<&str>, candidates: &[&Path], project: &Path, project_canon: Option<&Path>) -> bool {
    if candidates.len() <= 1 {
        return true;
    }
    let by_cwd = cwd.and_then(|c| {
        let c = Path::new(c);
        candidates
            .iter()
            .filter(|cand| path_matches(c, cand, project, project_canon))
            .max_by_key(|cand| cand.components().count())
    });
    by_cwd.or_else(|| candidates.first()).is_some_and(|owner| **owner == *project)
}

fn collect_folder(
    cache: &mut SessionIndexCache,
    dir: &Path,
    result: &mut SessionsResult,
    keep: impl Fn(&SessionSummary) -> bool,
) -> Result<(), SessionError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            cache.prune_dir(dir, &HashSet::new());
            return Ok(());
        }
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut seen = HashSet::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                result.unreadable.push(UnreadableSession { file_name: "?".into(), reason: e.to_string() });
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        seen.insert(path.to_string_lossy().into_owned());
        match cache.get_or_parse(&path) {
            Ok(s) if keep(&s) => result.sessions.push(s),
            Ok(_) => {}
            Err(e) => result.unreadable.push(UnreadableSession {
                file_name: entry.file_name().to_string_lossy().into_owned(),
                reason: e.to_string(),
            }),
        }
    }
    cache.prune_dir(dir, &seen);
    Ok(())
}

pub fn list_sessions_scoped(
    cache: &mut SessionIndexCache,
    claude_projects_dir: &Path,
    project_path: &Path,
    scope: ListScope<'_>,
) -> Result<SessionsResult, SessionError> {
    let encoded = encode_project_dir(project_path);
    let canon = fs::canonicalize(project_path).ok();
    let canon = canon.as_deref();
    let mut result = SessionsResult { sessions: Vec::new(), degraded_count: 0, unreadable: Vec::new() };

    let mut candidates: Vec<&Path> = vec![project_path];
    candidates.extend(
        scope
            .known
            .iter()
            .map(PathBuf::as_path)
            .filter(|q| *q != project_path && encode_project_dir(q) == encoded),
    );
    candidates.sort();
    candidates.dedup();
    collect_folder(cache, &claude_projects_dir.join(&encoded), &mut result, |s| {
        owns_session(s.cwd.as_deref(), &candidates, project_path, canon)
    })?;

    if scope.aggregate_children {
        // Subpastas que não são projeto: as pastas `<enc>-...` cujo `cwd` real
        // está sob o projeto e não pertence a um projeto conhecido mais fundo.
        let prefix = format!("{encoded}-");
        let deeper: Vec<&Path> = scope
            .known
            .iter()
            .map(PathBuf::as_path)
            .filter(|q| *q != project_path && q.starts_with(project_path))
            .collect();
        let mut folders: Vec<PathBuf> = fs::read_dir(claude_projects_dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        folders.sort();
        for folder in folders {
            collect_folder(cache, &folder, &mut result, |s| {
                s.cwd.as_deref().is_some_and(|c| {
                    let c = Path::new(c);
                    path_matches(c, project_path, project_path, canon) && !deeper.iter().any(|q| c.starts_with(q))
                })
            })?;
        }
    }

    result.sessions.sort_by(|a, b| {
        let ka = a.last_timestamp_ms.unwrap_or(a.file_modified_ms);
        let kb = b.last_timestamp_ms.unwrap_or(b.file_modified_ms);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
    result.unreadable.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    result.degraded_count = u32::try_from(result.sessions.iter().filter(|s| s.degraded).count()).unwrap_or(u32::MAX);
    Ok(result)
}

/// Escopo padrão: só o próprio projeto (sem detecção de colisão nem agregação).
pub fn list_sessions_in(
    cache: &mut SessionIndexCache,
    claude_projects_dir: &Path,
    project_path: &Path,
) -> Result<SessionsResult, SessionError> {
    list_sessions_scoped(cache, claude_projects_dir, project_path, ListScope::default())
}

/// Variante com caminho de cache explícito (`None` = sem cache em disco).
pub fn list_sessions_cached_scoped(
    claude_projects_dir: &Path,
    project_path: &Path,
    cache_path: Option<&Path>,
    scope: ListScope<'_>,
) -> Result<SessionsResult, SessionError> {
    let mut cache = SessionIndexCache::load(cache_path);
    let result = list_sessions_scoped(&mut cache, claude_projects_dir, project_path, scope);
    cache.save(cache_path);
    result
}

pub fn list_sessions_cached(
    claude_projects_dir: &Path,
    project_path: &Path,
    cache_path: Option<&Path>,
) -> Result<SessionsResult, SessionError> {
    list_sessions_cached_scoped(claude_projects_dir, project_path, cache_path, ListScope::default())
}

pub fn list_sessions(claude_projects_dir: &Path, project_path: &Path) -> Result<SessionsResult, SessionError> {
    list_sessions_for(claude_projects_dir, project_path, ListScope::default())
}

/// Com o cache padrão em disco e o escopo informado.
pub fn list_sessions_for(
    claude_projects_dir: &Path,
    project_path: &Path,
    scope: ListScope<'_>,
) -> Result<SessionsResult, SessionError> {
    list_sessions_cached_scoped(claude_projects_dir, project_path, cache_file().as_deref(), scope)
}

/// Vale agregar as sessões de subpastas ao projeto? Só se ele tem marcador
/// (`.git`/`CLAUDE.md`) e não é uma raiz (hub).
pub fn aggregates_children(config: &Config, project: &Path) -> bool {
    if !(project.join(".git").exists() || project.join("CLAUDE.md").is_file()) {
        return false;
    }
    let canon = fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    !config
        .roots
        .iter()
        .any(|r| fs::canonicalize(r).is_ok_and(|rc| rc == canon))
}

/// `exp(-idade_dias/14) * ln(1+mensagens)` para sessões dos últimos 30 dias.
pub(crate) fn session_weight(now_ms: i64, at_ms: i64, message_count: u32) -> f64 {
    let age_days = (now_ms.saturating_sub(at_ms).max(0) as f64) / DAY_MS;
    if age_days > SCORE_WINDOW_DAYS {
        return 0.0;
    }
    (-age_days / SCORE_HALF_LIFE_DAYS).exp() * f64::from(message_count).ln_1p()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(n: u32) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sessions")
            .join(format!("00000000-0000-4000-8000-{n:012}.jsonl"))
    }

    #[test]
    fn codifica_diretorio() {
        let p = Path::new("/home/u/workspace/areas/Pessoal/Organização/brAIn");
        assert_eq!(encode_project_dir(p), "-home-u-workspace-areas-Pessoal-Organiza--o-brAIn");
        assert_eq!(encode_project_dir(Path::new("/home/u/.local/x_y")), "-home-u--local-x-y");
    }

    #[test]
    fn valida_session_id() {
        assert!(is_valid_session_id("00000000-0000-4000-8000-000000000001"));
        assert!(is_valid_session_id("ABCDEF01-0000-4000-8000-00000000000A"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("--dangerously-skip-permissions"));
        assert!(!is_valid_session_id("--00000-0000-4000-8000-000000000001"));
        assert!(!is_valid_session_id("00000000-0000-4000-8000-0000000000012"));
        assert!(!is_valid_session_id("00000000-0000-4000-8000-00000000000g"));
        assert!(!is_valid_session_id("00000000000040008000000000000001----"));
        assert!(!is_valid_session_id("00000000-0000-4000-8000-000000000001 "));
    }

    #[test]
    fn timestamp_iso() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso_ms("2026-10-11T14:31:57.234Z"), Some(1_791_729_117_234));
        assert_eq!(parse_iso_ms("lixo"), None);
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2026-10-11T14:31:57.2x4Z"), None);
    }

    #[test]
    fn sessao_completa() {
        let s = parse_session_file(&fixture(1)).unwrap();
        assert_eq!(s.id, "00000000-0000-4000-8000-000000000001");
        assert_eq!(s.title, "Título final da sessão");
        assert_eq!(s.title_source, TitleSource::AiTitle);
        // 2 prompts humanos (meta, injeção <command-name>, tool_result e sidechain fora) + 2 respostas distintas
        assert_eq!(s.message_count, 4);
        assert_eq!(s.cwd.as_deref(), Some("/tmp/proj/alpha"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.first_timestamp_ms, parse_iso_ms("2026-10-01T10:00:00.000Z"));
        assert_eq!(s.last_timestamp_ms, parse_iso_ms("2026-10-01T10:05:00.000Z"));
        assert!(!s.degraded);
        assert_eq!(s.skipped_lines, 0);
        assert!(!s.active);
    }

    #[test]
    fn titulo_por_last_prompt_e_primeiro_prompt() {
        let s = parse_session_file(&fixture(2)).unwrap();
        assert_eq!(s.title_source, TitleSource::LastPrompt);
        assert_eq!(s.title, "Resumo do último prompt");
        let s = parse_session_file(&fixture(9)).unwrap();
        assert_eq!(s.title_source, TitleSource::AiTitle);
        assert_eq!(s.title, "Segundo título");
    }

    #[test]
    fn primeiro_prompt_ignora_injecao_e_colapsa() {
        let s = parse_session_file(&fixture(1)).unwrap();
        // fixture 1 tem ai-title; testa o acumulador diretamente sem ele
        let mut acc = Accumulator::default();
        acc.feed(br#"{"type":"user","message":{"content":"<system-reminder>x</system-reminder>"}}"#);
        acc.feed(format!(r#"{{"type":"user","message":{{"content":"  {}\n  fim"}}}}"#, "á".repeat(200)).as_bytes());
        let r = acc.finish("x".into(), 0, 0).unwrap();
        assert_eq!(r.title_source, TitleSource::FirstPrompt);
        assert_eq!(r.title.chars().count(), 80);
        // a injeção <system-reminder> NÃO conta como prompt humano
        assert_eq!(r.message_count, 1);
        assert_eq!(s.message_count, 4);
    }

    #[test]
    fn linha_corrompida_e_truncada_degradam() {
        let s = parse_session_file(&fixture(3)).unwrap();
        assert!(s.degraded);
        assert_eq!(s.skipped_lines, 1);
        assert_eq!(s.message_count, 2);
        let s = parse_session_file(&fixture(4)).unwrap();
        assert!(s.degraded);
        assert_eq!(s.skipped_lines, 1);
        assert_eq!(s.message_count, 2);
    }

    #[test]
    fn vazio_e_erro_tipado() {
        assert!(matches!(parse_session_file(&fixture(5)), Err(SessionError::Empty)));
    }

    #[test]
    fn so_meta_nao_tem_titulo() {
        let s = parse_session_file(&fixture(6)).unwrap();
        assert_eq!(s.title, UNTITLED);
        assert_eq!(s.title_source, TitleSource::None);
        assert_eq!(s.message_count, 0);
    }

    #[test]
    fn cwd_com_acento() {
        let s = parse_session_file(&fixture(7)).unwrap();
        assert_eq!(s.cwd.as_deref(), Some("/tmp/proj/Organização/brAIn"));
        assert_eq!(s.title, "Olá, mundo — ação");
    }

    #[test]
    fn utf8_invalido_pula_linha() {
        let s = parse_session_file(&fixture(8)).unwrap();
        assert!(s.degraded);
        assert_eq!(s.skipped_lines, 1);
        assert_eq!(s.message_count, 2);
    }

    #[test]
    fn linha_gigante_e_pulada() {
        let mut data: &[u8] = b"{\"type\":\"user\"}\n0123456789\n{\"type\":\"user\"}";
        let mut buf = Vec::new();
        let mut got = Vec::new();
        while let Some(ok) = read_line_limited(&mut data, &mut buf, 16).unwrap() {
            got.push(ok);
        }
        assert_eq!(got, vec![true, true, true]);
        let mut data: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nok\n";
        got.clear();
        while let Some(ok) = read_line_limited(&mut data, &mut buf, 8).unwrap() {
            got.push(ok);
        }
        assert_eq!(got, vec![false, true]);
    }

    #[test]
    fn nome_de_arquivo_invalido() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nao-e-uuid.jsonl");
        fs::write(&p, "{}\n").unwrap();
        assert!(matches!(parse_session_file(&p), Err(SessionError::InvalidFileName(_))));
    }

    fn montar(project: &Path) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(encode_project_dir(project));
        fs::create_dir_all(dir.join("subagents")).unwrap();
        fs::write(dir.join("subagents/x.jsonl"), "{}\n").unwrap();
        for n in [1, 2, 3, 5, 6, 8] {
            fs::copy(fixture(n), dir.join(fixture(n).file_name().unwrap())).unwrap();
        }
        fs::write(dir.join("notas.txt"), "x").unwrap();
        (root, dir)
    }

    #[test]
    fn lista_filtra_ordena_e_reporta_ilegiveis() {
        let project = Path::new("/tmp/proj/alpha");
        let (root, dir) = montar(project);
        // sessão de outro projeto que colide na codificação (cwd diferente)
        let colide = dir.join("00000000-0000-4000-8000-0000000000ff.jsonl");
        fs::write(&colide, "{\"type\":\"user\",\"cwd\":\"/tmp/proj-alpha\",\"timestamp\":\"2026-10-09T00:00:00Z\"}\n").unwrap();
        // com o projeto colidente conhecido, o cwd desempata
        let known = [PathBuf::from("/tmp/proj-alpha")];
        let scope = ListScope { known: &known, aggregate_children: false };
        let r = list_sessions_cached_scoped(root.path(), project, None, scope).unwrap();
        let ids: Vec<_> = r.sessions.iter().map(|s| s.id.rsplit('-').next().unwrap().to_string()).collect();
        assert_eq!(r.unreadable.len(), 1);
        assert!(r.unreadable[0].file_name.ends_with("0005.jsonl"));
        assert!(!ids.iter().any(|i| i.ends_with("ff")));
        assert_eq!(r.sessions.len(), 5);
        let keys: Vec<_> = r.sessions.iter().map(|s| s.last_timestamp_ms.unwrap_or(s.file_modified_ms)).collect();
        assert!(keys.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(r.degraded_count, 2);
        // projeto sem diretório → vazio, sem erro
        let vazio = list_sessions_cached(root.path(), Path::new("/nada/aqui"), None).unwrap();
        assert!(vazio.sessions.is_empty());
    }

    #[test]
    fn cache_evita_reparse_e_invalida_por_mudanca() {
        let project = Path::new("/tmp/proj/alpha");
        let (root, dir) = montar(project);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = cache_dir.path().join("sub/idx.json");
        let first = list_sessions_cached(root.path(), project, Some(&cache)).unwrap();
        assert!(cache.exists());
        // adultera o resumo em cache: um hit o devolve sem reparsear
        let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        for e in raw["entries"].as_object_mut().unwrap().values_mut() {
            e["summary"]["title"] = "DO CACHE".into();
        }
        fs::write(&cache, serde_json::to_vec(&raw).unwrap()).unwrap();
        let hit = list_sessions_cached(root.path(), project, Some(&cache)).unwrap();
        assert!(hit.sessions.iter().all(|s| s.title == "DO CACHE"));
        // arquivo mudou (size) → reparseia
        let f1 = dir.join(fixture(1).file_name().unwrap());
        let mut c = fs::read_to_string(&f1).unwrap();
        c.push_str("{\"type\":\"user\",\"cwd\":\"/tmp/proj/alpha\",\"message\":{\"content\":\"mais um\"}}\n");
        fs::write(&f1, c).unwrap();
        let miss = list_sessions_cached(root.path(), project, Some(&cache)).unwrap();
        let s1 = miss.sessions.iter().find(|s| s.id.ends_with("0001")).unwrap();
        assert_eq!(s1.title, "Título final da sessão");
        assert_eq!(s1.message_count, 5);
        assert_eq!(first.sessions.len(), miss.sessions.len());
        // cache corrompido nunca é fatal
        fs::write(&cache, "lixo").unwrap();
        assert!(list_sessions_cached(root.path(), project, Some(&cache)).is_ok());
    }

    #[test]
    fn pasta_codificada_manda_e_cwd_so_desempata() {
        let project = Path::new("/tmp/proj/alpha");
        let (root, dir) = montar(project);
        // sem projeto colidente conhecido, cwd antigo/renomeado NÃO exclui a sessão
        fs::write(
            dir.join("00000000-0000-4000-8000-0000000000fe.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/tmp/outro/lugar\",\"timestamp\":\"2026-10-09T00:00:00Z\"}\n",
        )
        .unwrap();
        let r = list_sessions_cached(root.path(), project, None).unwrap();
        assert!(r.sessions.iter().any(|s| s.id.ends_with("00fe")));
        // sessão sem cwd em pasta colidente fica só com o menor caminho
        let sem = dir.join("00000000-0000-4000-8000-0000000000fd.jsonl");
        fs::write(&sem, "{\"type\":\"ai-title\",\"aiTitle\":\"sem cwd\"}\n").unwrap();
        let outro = PathBuf::from("/tmp/proj-alpha");
        let known = [project.to_path_buf(), outro.clone()];
        let has = |p: &Path| {
            let scope = ListScope { known: &known, aggregate_children: false };
            let mut c = SessionIndexCache::empty();
            list_sessions_scoped(&mut c, root.path(), p, scope).unwrap().sessions.iter().any(|s| s.id.ends_with("00fd"))
        };
        // ordem de Path (por componente): "/tmp/proj/alpha" < "/tmp/proj-alpha"
        assert!(has(project));
        assert!(!has(&outro));
    }

    #[test]
    fn agrega_subpastas_sem_engolir_projeto_mais_fundo() {
        let root = tempfile::tempdir().unwrap();
        let proj = Path::new("/tmp/agg/proj");
        let sub = Path::new("/tmp/agg/proj/src");
        let deep = Path::new("/tmp/agg/proj/lib");
        let irmao = Path::new("/tmp/agg/proj-x");
        for (n, cwd) in [(1u32, proj), (2, sub), (3, deep), (4, irmao)] {
            let d = root.path().join(encode_project_dir(cwd));
            fs::create_dir_all(&d).unwrap();
            let line = format!("{{\"type\":\"user\",\"cwd\":\"{}\",\"timestamp\":\"2026-10-01T00:00:00Z\"}}\n", cwd.display());
            fs::write(d.join(format!("00000000-0000-4000-8000-{n:012}.jsonl")), line).unwrap();
        }
        let known = [proj.to_path_buf(), deep.to_path_buf(), irmao.to_path_buf()];
        let ids = |agg: bool| {
            let scope = ListScope { known: &known, aggregate_children: agg };
            let mut c = SessionIndexCache::empty();
            let mut v: Vec<String> = list_sessions_scoped(&mut c, root.path(), proj, scope)
                .unwrap()
                .sessions
                .into_iter()
                .map(|s| s.id[s.id.len() - 2..].to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids(false), vec!["01"]);
        // agrega proj/src (02); não pega proj/lib (projeto conhecido) nem proj-x (irmão)
        assert_eq!(ids(true), vec!["01", "02"]);
    }

    #[test]
    fn iso_offsets_calendario_e_lixo() {
        assert_eq!(parse_iso_ms("2024-01-01T03:00:00+03:00"), Some(1_704_067_200_000));
        assert_eq!(parse_iso_ms("2024-01-01T00:00:00-0000"), Some(1_704_067_200_000));
        assert_eq!(parse_iso_ms("2023-02-29T00:00:00Z"), None);
        assert!(parse_iso_ms("2000-02-29T00:00:00Z").is_some());
        assert_eq!(parse_iso_ms("1900-02-29T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("0000-01-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2024-+1-+1T+0:+0:+0Z"), None);
        assert_eq!(parse_iso_ms("999999999999999-01-01T00:00:00Z"), None);
    }

    #[test]
    fn encode_emoji_utf16() {
        assert_eq!(encode_project_dir(Path::new("/a/😀")), "-a---");
    }

    #[test]
    fn campos_com_tipo_errado_e_compact_summary() {
        let mut acc = Accumulator::default();
        acc.feed(br#"{"type":"user","timestamp":123,"cwd":5,"isMeta":"true","message":{"content":"oi"}}"#);
        acc.feed(br#"{"type":"user","isCompactSummary":true,"message":{"content":"resumo"}}"#);
        acc.feed(b"\xEF\xBB\xBF{\"type\":\"user\",\"message\":{\"content\":\"[Request interrupted by user]\"}}");
        assert_eq!(acc.skipped, 0);
        assert_eq!(acc.prompts, 1);
    }
}
