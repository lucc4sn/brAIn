//! Testes adversariais (independentes) do backend do brAIn.
//! Só dados sintéticos em tempdirs. Nada de ~/.claude real nem .env.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use brain_lib::config::{Config, ConfigError, ProjectMeta, Region, TerminalConfig, TerminalKind};
use brain_lib::error::AppError;
use brain_lib::git::{self, GitHead};
use brain_lib::launcher::{self, LaunchError, LaunchMode};
use brain_lib::sessions::{
    self, SessionError, SessionIndexCache, SessionSummary, SessionsResult, TitleSource, UnreadableSession,
};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers

fn uid(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-000000000000")
}

fn user_line(ts: &str, cwd: &str, text: &str) -> String {
    json!({"type":"user","timestamp":ts,"cwd":cwd,"message":{"role":"user","content":text}}).to_string()
}

fn asst_line(ts: &str, req: &str) -> String {
    json!({"type":"assistant","timestamp":ts,"requestId":req,"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}})
        .to_string()
}

fn proc_kb(key: &str) -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

fn reset_hwm() {
    let _ = fs::write("/proc/self/clear_refs", "5");
}

/// Escreve `lines` num arquivo de sessão válido e devolve (tempdir, caminho).
fn one_session(content: &[u8]) -> (TempDir, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join(format!("{}.jsonl", uid(1)));
    fs::write(&p, content).unwrap();
    (td, p)
}

fn parse_lines(lines: &[&str]) -> Result<SessionSummary, SessionError> {
    let (_td, p) = one_session(lines.join("\n").as_bytes());
    sessions::parse_session_file(&p)
}

struct Env {
    _td: TempDir,
    cp: PathBuf,   // ~/.claude/projects sintético
    proj: PathBuf, // diretório do projeto
    sdir: PathBuf, // cp/<encode(proj)>
}

fn env_with_project(name: &str) -> Env {
    let td = tempfile::tempdir().unwrap();
    let cp = td.path().join("cp");
    let proj = td.path().join(name);
    fs::create_dir_all(&cp).unwrap();
    fs::create_dir_all(&proj).unwrap();
    let sdir = cp.join(sessions::encode_project_dir(&proj));
    fs::create_dir_all(&sdir).unwrap();
    Env { _td: td, cp, proj, sdir }
}

fn list(env: &Env) -> SessionsResult {
    sessions::list_sessions_in(&mut SessionIndexCache::empty(), &env.cp, &env.proj).unwrap()
}

fn write_giant(path: &Path, head: &str, size: usize, tail: &str) {
    let mut f = File::create(path).unwrap();
    f.write_all(head.as_bytes()).unwrap();
    let chunk = vec![b'x'; 1 << 20];
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len());
        f.write_all(&chunk[..n]).unwrap();
        left -= n;
    }
    f.write_all(b"\n").unwrap();
    f.write_all(tail.as_bytes()).unwrap();
}

// ============================================================ SESSIONS

#[test]
fn s_giant_line_50mb_is_skipped_with_bounded_memory() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join(format!("{}.jsonl", uid(1)));
    let head = format!("{}\n", user_line("2024-05-01T10:00:00Z", "/x", "antes"));
    let tail = format!("{}\n", user_line("2024-05-01T10:00:01Z", "/x", "depois"));
    write_giant(&p, &head, 50 * 1024 * 1024 + 1, &tail);
    reset_hwm();
    let rss0 = proc_kb("VmRSS:");
    let t = Instant::now();
    let s = sessions::parse_session_file(&p).unwrap();
    let el = t.elapsed();
    let hwm = proc_kb("VmHWM:");
    eprintln!("[VOLUME] linha 50MiB: {el:?}; RSS antes={rss0} kB, pico={hwm} kB, delta={} kB", hwm.saturating_sub(rss0));
    assert_eq!(s.skipped_lines, 1, "linha gigante deve contar como pulada");
    assert!(s.degraded);
    assert_eq!(s.message_count, 2, "as linhas válidas antes/depois devem contar");
    // VmHWM é do processo inteiro: só é confiável com BRAIN_MEM_ASSERT=1 e --test-threads=1.
    if std::env::var_os("BRAIN_MEM_ASSERT").is_some() {
        assert!(hwm.saturating_sub(rss0) < 64 * 1024, "pico de memória > 64MiB acima da base");
    }
}

#[test]
fn s_giant_line_alone_observed() {
    // Só a linha gigante: sem nenhuma linha utilizável.
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join(format!("{}.jsonl", uid(1)));
    write_giant(&p, "", 50 * 1024 * 1024 + 1, "");
    let r = sessions::parse_session_file(&p);
    eprintln!("[OBS] arquivo só com linha gigante -> {r:?}");
    // Não deve estourar nem panicar; Err(Empty) é aceitável (vai p/ unreadable).
    assert!(matches!(r, Err(SessionError::Empty)) || r.is_ok());
}

#[test]
fn s_line_size_boundary() {
    // Linha de exatamente 8 MiB de conteúdo + '\n' vs 8MiB-1: define de que lado cai o limite.
    let max = 8 * 1024 * 1024;
    let td = tempfile::tempdir().unwrap();
    let mk = |n: usize, name: u32| {
        let p = td.path().join(format!("{}.jsonl", uid(name)));
        let companion = user_line("2024-05-01T10:00:00Z", "/x", "companheira");
        let pad = n - r#"{"type":"user","cwd":"/x","pad":""}"#.len();
        let line = format!(r#"{{"type":"user","cwd":"/x","pad":"{}"}}"#, "a".repeat(pad));
        assert_eq!(line.len(), n);
        fs::write(&p, format!("{companion}\n{line}\n")).unwrap();
        sessions::parse_session_file(&p).unwrap()
    };
    let under = mk(max - 1, 1); // com \n = max -> aceito
    let exact = mk(max, 2); // com \n = max+1 -> pulado?
    eprintln!("[OBS] limite: conteúdo=max-1 skipped={}, conteúdo=max skipped={}", under.skipped_lines, exact.skipped_lines);
    assert_eq!(under.skipped_lines, 0);
    assert_eq!(exact.skipped_lines, 0, "linha com exatamente 8 MiB de conteúdo (sem o \\n) foi pulada: limite conta o \\n");
}

#[test]
fn s_many_valid_lines_perf() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join(format!("{}.jsonl", uid(1)));
    let mut f = std::io::BufWriter::new(File::create(&p).unwrap());
    let n = 100_000;
    for i in 0..n {
        writeln!(f, "{}", user_line("2024-05-01T10:00:00.123Z", "/x", &format!("pergunta {i}"))).unwrap();
        writeln!(f, "{}", asst_line("2024-05-01T10:00:01.000Z", &format!("req{i}"))).unwrap();
    }
    drop(f);
    let size = fs::metadata(&p).unwrap().len();
    reset_hwm();
    let rss0 = proc_kb("VmRSS:");
    let t = Instant::now();
    let s = sessions::parse_session_file(&p).unwrap();
    let el = t.elapsed();
    eprintln!(
        "[VOLUME] {} linhas ({} MiB) em {el:?} (build debug); delta RSS pico={} kB",
        2 * n,
        size >> 20,
        proc_kb("VmHWM:").saturating_sub(rss0)
    );
    assert_eq!(s.message_count, 2 * n);
    assert_eq!(s.skipped_lines, 0);
    assert!(el < Duration::from_secs(20));
}

#[test]
fn s_bom_utf8() {
    let l1 = user_line("2024-05-01T10:00:00Z", "/x", "primeira");
    let l2 = user_line("2024-05-01T10:00:01Z", "/x", "segunda");
    let content = format!("\u{feff}{l1}\n{l2}\n");
    let (_td, p) = one_session(content.as_bytes());
    let s = sessions::parse_session_file(&p).unwrap();
    assert_eq!(s.skipped_lines, 0, "linha com BOM foi pulada");
    assert_eq!(s.message_count, 2);
}

#[test]
fn s_crlf() {
    let l1 = user_line("2024-05-01T10:00:00Z", "/x", "primeira");
    let l2 = asst_line("2024-05-01T10:00:01Z", "r1");
    let (_td, p) = one_session(format!("{l1}\r\n{l2}\r\n").as_bytes());
    let s = sessions::parse_session_file(&p).unwrap();
    assert_eq!(s.skipped_lines, 0);
    assert_eq!(s.message_count, 2);
}

#[test]
fn s_no_trailing_newline() {
    let l1 = user_line("2024-05-01T10:00:00Z", "/x", "primeira");
    let l2 = asst_line("2024-05-01T10:00:01Z", "r1");
    let (_td, p) = one_session(format!("{l1}\n{l2}").as_bytes());
    let s = sessions::parse_session_file(&p).unwrap();
    assert_eq!(s.skipped_lines, 0);
    assert_eq!(s.message_count, 2);
}

#[test]
fn s_truncated_last_line_is_degraded_not_error() {
    let l1 = user_line("2024-05-01T10:00:00Z", "/x", "primeira");
    let (_td, p) = one_session(format!("{l1}\n{{\"type\":\"assistant\",\"requ").as_bytes());
    let s = sessions::parse_session_file(&p).unwrap();
    assert_eq!(s.skipped_lines, 1);
    assert!(s.degraded);
    assert_eq!(s.message_count, 1);
}

#[test]
fn s_empty_and_blank_files_are_errors() {
    for c in ["", "\n\n  \n", "   "] {
        let (_td, p) = one_session(c.as_bytes());
        assert!(matches!(sessions::parse_session_file(&p), Err(SessionError::Empty)), "conteúdo {c:?}");
    }
}

#[test]
fn s_invalid_utf8_line_skipped() {
    let l1 = user_line("2024-05-01T10:00:00Z", "/x", "primeira");
    let mut bytes = l1.into_bytes();
    bytes.extend_from_slice(b"\n\xff\xfe\xfd{\"type\":\"user\"}\n");
    let (_td, p) = one_session(&bytes);
    let s = sessions::parse_session_file(&p).unwrap();
    assert_eq!(s.skipped_lines, 1);
}

#[test]
fn s_non_object_json_lines_skipped() {
    let l = user_line("2024-05-01T10:00:00Z", "/x", "ok");
    let s = parse_lines(&["[]", "null", "42", "\"str\"", "true", &l]).unwrap();
    assert_eq!(s.skipped_lines, 5);
    assert_eq!(s.message_count, 1);
}

#[test]
fn s_wrong_type_timestamp_numeric_does_not_discard_line() {
    let ok = user_line("2024-05-01T10:00:00Z", "/x", "companheira");
    let s = parse_lines(&[&ok, r#"{"type":"user","timestamp":1700000000000,"message":{"content":"oi"}}"#]).unwrap();
    assert_eq!(s.skipped_lines, 0, "timestamp numérico descartou a linha inteira");
    assert_eq!(s.message_count, 2);
}

#[test]
fn s_wrong_type_message_string() {
    let s = parse_lines(&[r#"{"type":"user","timestamp":"2024-05-01T10:00:00Z","message":"oops"}"#]).unwrap();
    assert_eq!(s.skipped_lines, 0);
    assert_eq!(s.message_count, 0);
}

#[test]
fn s_wrong_type_content_null_and_number() {
    let s = parse_lines(&[
        r#"{"type":"user","message":{"content":null}}"#,
        r#"{"type":"user","message":{"content":42}}"#,
        r#"{"type":"user","message":{"content":[1,2,{"type":"text"}]}}"#,
    ]);
    // Aceitável: usável sem crash. Observa se a 3a linha (blocos heterogêneos) foi pulada.
    let s = s.unwrap();
    eprintln!("[OBS] content null/number/blocos heterogêneos: skipped={}", s.skipped_lines);
    assert!(s.skipped_lines <= 1, "só blocos heterogêneos poderiam ser pulados");
}

#[test]
fn s_wrong_type_is_meta_string() {
    let ok = user_line("2024-05-01T10:00:00Z", "/x", "companheira");
    let s = parse_lines(&[&ok, r#"{"type":"user","isMeta":"true","message":{"content":"x"}}"#]).unwrap();
    assert_eq!(s.skipped_lines, 0, "isMeta string descartou a linha");
}

#[test]
fn s_wrong_type_cwd_and_type_numeric() {
    let s = parse_lines(&[
        r#"{"type":"user","cwd":123,"message":{"content":"x"}}"#,
        r#"{"type":5,"message":{"content":"x"}}"#,
        &user_line("2024-05-01T10:00:00Z", "/x", "ok"),
    ])
    .unwrap();
    assert_eq!(s.skipped_lines, 0, "campo com tipo errado descartou linha(s)");
}

#[test]
fn s_duplicate_request_ids_counted_once_and_sidechain_ignored() {
    let a = asst_line("2024-05-01T10:00:01Z", "same");
    let b = asst_line("2024-05-01T10:00:02Z", "same");
    let side = r#"{"type":"assistant","isSidechain":true,"requestId":"z","message":{"content":"x"}}"#;
    let s = parse_lines(&[&a, &b, side]).unwrap();
    assert_eq!(s.message_count, 1);
}

#[test]
fn s_title_truncation_unicode_no_panic() {
    let text = "ü😀".repeat(200);
    let l = user_line("2024-05-01T10:00:00Z", "/x", &text);
    let s = parse_lines(&[&l]).unwrap();
    assert_eq!(s.title.chars().count(), 80);
    let l = user_line("2024-05-01T10:00:00Z", "/x", "<system-reminder>blah</system-reminder>");
    let l2 = user_line("2024-05-01T10:00:01Z", "/x", "pergunta real");
    let s = parse_lines(&[&l, &l2]).unwrap();
    assert_eq!(s.title, "pergunta real");
    assert_eq!(s.title_source, TitleSource::FirstPrompt);
}

#[test]
fn s_iso_timestamps() {
    use sessions::parse_iso_ms as p;
    assert_eq!(p("2024-01-01T00:00:00Z"), Some(1_704_067_200_000));
    assert_eq!(p("2024-01-01T00:00:00.5Z"), Some(1_704_067_200_500));
    assert_eq!(p("2024-01-01T00:00:00.123456789Z"), Some(1_704_067_200_123));
    assert_eq!(p("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
    assert_eq!(p("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(p("2262-04-11T23:47:16.854Z"), Some(9_223_372_036_854));
    assert_eq!(p("2024-13-01T00:00:00Z"), None);
    assert_eq!(p("2024-00-10T00:00:00Z"), None);
    assert_eq!(p("2024-01-01T24:00:00Z"), None);
    assert_eq!(p(""), None);
    assert_eq!(p("garbage"), None);
    assert_eq!(p("2024-01-01"), None);
}

#[test]
fn s_iso_with_offset_plus_0000() {
    // Formato ISO válido que o parser rejeita: +00:00 em vez de Z.
    assert_eq!(sessions::parse_iso_ms("2024-01-01T00:00:00+00:00"), Some(1_704_067_200_000));
    assert_eq!(sessions::parse_iso_ms("2024-01-01T03:00:00+03:00"), Some(1_704_067_200_000));
}

#[test]
fn s_iso_impossible_calendar_dates_rejected() {
    use sessions::parse_iso_ms as p;
    assert_eq!(p("2023-02-29T00:00:00Z"), None, "2023 não é bissexto");
    assert_eq!(p("2024-02-30T00:00:00Z"), None);
    assert_eq!(p("2024-04-31T00:00:00Z"), None);
    assert_eq!(p("1900-02-29T00:00:00Z"), None, "1900 não é bissexto");
}

#[test]
fn s_iso_sign_garbage_rejected() {
    use sessions::parse_iso_ms as p;
    assert_eq!(p("2024-+1-+1T+0:+0:+0Z"), None);
    assert_eq!(p("-2024-01-01T00:00:00Z"), None);
}

#[test]
fn s_iso_huge_year_does_not_panic() {
    let r = std::panic::catch_unwind(|| sessions::parse_iso_ms("999999999999999-01-01T00:00:00Z"));
    match r {
        Err(_) => panic!("parse_iso_ms entrou em pânico (overflow aritmético) com ano gigante"),
        Ok(v) => assert_eq!(v, None, "ano absurdo deveria ser rejeitado, retornou {v:?}"),
    }
}

#[test]
fn s_iso_year_far_future_ordering() {
    // 2262 e 9999 devem ordenar corretamente (sem wrap).
    let a = sessions::parse_iso_ms("2262-04-11T23:47:16Z").unwrap();
    let b = sessions::parse_iso_ms("9999-12-31T23:59:59Z").unwrap();
    assert!(b > a && a > 0);
}

#[test]
fn s_cwd_with_spaces_newline_unicode() {
    for name in ["proj com espaço", "proj\nnova linha", "proj-ü-ñ-日本", "  lead trail  "] {
        let env = env_with_project(name);
        let cwd = env.proj.to_string_lossy().into_owned();
        let l = user_line("2024-05-01T10:00:00Z", &cwd, "oi");
        fs::write(env.sdir.join(format!("{}.jsonl", uid(1))), l).unwrap();
        let r = list(&env);
        assert_eq!(r.sessions.len(), 1, "nome {name:?}: {r:?}");
        assert_eq!(r.sessions[0].cwd.as_deref(), Some(cwd.as_str()));
    }
}

#[test]
fn s_filenames_and_special_entries() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let l = user_line("2024-05-01T10:00:00Z", &cwd, "oi");
    // válido (maiúsculo)
    let upper = "ABCDEF01-0000-4000-8000-00000000000A";
    fs::write(env.sdir.join(format!("{upper}.jsonl")), &l).unwrap();
    // nome não-UUID
    fs::write(env.sdir.join("notes.jsonl"), &l).unwrap();
    // diretório chamado *.jsonl
    fs::create_dir(env.sdir.join(format!("{}.jsonl", uid(3)))).unwrap();
    // symlink quebrado
    symlink("/nonexistent/zzz", env.sdir.join(format!("{}.jsonl", uid(4)))).unwrap();
    // symlink para arquivo fora
    let outside = env.proj.join("outside.jsonl");
    fs::write(&outside, &l).unwrap();
    symlink(&outside, env.sdir.join(format!("{}.jsonl", uid(5)))).unwrap();
    // extensão diferente
    fs::write(env.sdir.join(format!("{}.json", uid(6))), &l).unwrap();
    let r = list(&env);
    let ids: Vec<_> = r.sessions.iter().map(|s| s.id.as_str()).collect();
    eprintln!("[OBS] ids={ids:?} unreadable={:?}", r.unreadable);
    assert!(ids.contains(&upper), "UUID em maiúsculas deveria ser aceito");
    assert_eq!(r.unreadable.len(), 1);
    assert_eq!(r.unreadable[0].file_name, "notes.jsonl");
    assert!(!ids.contains(&uid(5).as_str()), "symlink para fora foi seguido");
    assert_eq!(r.sessions.len(), 1);
}

#[test]
fn s_unreadable_permission_reported_others_ok() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let l = user_line("2024-05-01T10:00:00Z", &cwd, "oi");
    fs::write(env.sdir.join(format!("{}.jsonl", uid(1))), &l).unwrap();
    let bad = env.sdir.join(format!("{}.jsonl", uid(2)));
    fs::write(&bad, &l).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
    let r = list(&env);
    assert_eq!(r.sessions.len(), 1);
    assert_eq!(r.unreadable.len(), 1);
    assert!(r.unreadable[0].file_name.starts_with("00000002"));
}

#[test]
fn s_missing_projects_dir_and_unreadable_dir() {
    let td = tempfile::tempdir().unwrap();
    let r = sessions::list_sessions_in(&mut SessionIndexCache::empty(), &td.path().join("nope"), &td.path().join("proj")).unwrap();
    assert!(r.sessions.is_empty() && r.unreadable.is_empty());
    // diretório da sessão sem permissão -> Err (não panic)
    let env = env_with_project("p");
    fs::set_permissions(&env.sdir, fs::Permissions::from_mode(0o000)).unwrap();
    let r = sessions::list_sessions_in(&mut SessionIndexCache::empty(), &env.cp, &env.proj);
    fs::set_permissions(&env.sdir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(r, Err(SessionError::Io { .. })));
}

#[test]
fn s_encode_project_dir_edge_cases() {
    use sessions::encode_project_dir as e;
    assert_eq!(e(Path::new("")), "");
    assert_eq!(e(Path::new("/")), "-");
    assert_eq!(e(Path::new("/a/b.c")), e(Path::new("/a/b-c")), "colisão conhecida");
    assert_eq!(e(Path::new("/a b")), "-a-b");
    assert_eq!(e(Path::new("/a/ü")), "-a--");
}

#[test]
fn s_encode_project_dir_emoji_utf16() {
    assert_eq!(sessions::encode_project_dir(Path::new("/a/😀")), "-a---");
}

#[test]
fn s_colliding_project_dirs_are_separated_by_cwd() {
    let td = tempfile::tempdir().unwrap();
    let cp = td.path().join("cp");
    let a = td.path().join("b.c");
    let b = td.path().join("b-c");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let sdir = cp.join(sessions::encode_project_dir(&a));
    assert_eq!(sdir, cp.join(sessions::encode_project_dir(&b)));
    fs::create_dir_all(&sdir).unwrap();
    fs::write(sdir.join(format!("{}.jsonl", uid(1))), user_line("2024-05-01T10:00:00Z", &a.to_string_lossy(), "do A")).unwrap();
    fs::write(sdir.join(format!("{}.jsonl", uid(2))), user_line("2024-05-01T10:00:00Z", &b.to_string_lossy(), "do B")).unwrap();
    // Mudou (item 13): o desempate por cwd exige o conjunto de projetos conhecidos (ListScope).
    let known = [a.clone(), b.clone()];
    let scope = sessions::ListScope { known: &known, aggregate_children: false };
    let mut c = SessionIndexCache::empty();
    let ra = sessions::list_sessions_scoped(&mut c, &cp, &a, scope).unwrap();
    let rb = sessions::list_sessions_scoped(&mut c, &cp, &b, scope).unwrap();
    assert_eq!(ra.sessions.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["do A"]);
    assert_eq!(rb.sessions.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["do B"]);
}

#[test]
#[ignore = "ACEITO: sem o conjunto de projetos conhecidos (list_sessions_in usa só o próprio projeto) não há como detectar a colisão; com ListScope.known a sessão sem cwd vai só ao menor caminho (teste unitário sessions::pasta_codificada_manda_e_cwd_so_desempata)"]
fn s_session_without_cwd_belongs_to_every_colliding_project() {
    // Observação: cwd=None => pertence a todos. Em diretórios colididos isso duplica a sessão.
    let td = tempfile::tempdir().unwrap();
    let cp = td.path().join("cp");
    let a = td.path().join("b.c");
    let b = td.path().join("b-c");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let sdir = cp.join(sessions::encode_project_dir(&a));
    fs::create_dir_all(&sdir).unwrap();
    fs::write(sdir.join(format!("{}.jsonl", uid(1))), r#"{"type":"ai-title","aiTitle":"sem cwd"}"#).unwrap();
    let mut c = SessionIndexCache::empty();
    let na = sessions::list_sessions_in(&mut c, &cp, &a).unwrap().sessions.len();
    let nb = sessions::list_sessions_in(&mut c, &cp, &b).unwrap().sessions.len();
    eprintln!("[OBS] sessão sem cwd aparece em b.c={na} e b-c={nb}");
    assert!(na + nb <= 1, "sessão sem cwd aparece em ambos os projetos colididos");
}

#[test]
fn s_is_valid_session_id_table() {
    let v = sessions::is_valid_session_id;
    assert!(v("00000000-0000-4000-8000-000000000001"));
    assert!(v("ABCDEF01-0000-4000-8000-00000000000A"));
    assert!(!v("--help"));
    assert!(!v("--dangerously-skip-permissions"));
    assert!(!v("00000000-0000-4000-8000-000000000001 "));
    assert!(!v(" 0000000-0000-4000-8000-000000000001"));
    assert!(!v("00000000-0000-4000-8000-00000000000\0"));
    assert!(!v("\0\0\0\0\0\0\0\0-\0\0\0\0-\0\0\0\0-\0\0\0\0-\0\0\0\0\0\0\0\0\0\0\0\0"));
    assert!(!v("00000000-0000-4000-8000-00000000000")); // 35
    assert!(!v("00000000-0000-4000-8000-0000000000001")); // 37
    assert!(!v("0000000g-0000-4000-8000-000000000001"));
    assert!(!v("00000000_0000_4000_8000_000000000001"));
    assert!(!v("0000000000000000000000000000000000000"));
    assert!(!v("ü0000000-0000-4000-8000-00000000000")); // 36 bytes, unicode
    assert!(!v("ａ0000000-0000-4000-8000-0000000000"));
    assert!(!v(&"a".repeat(10_000)));
    assert!(!v(""));
    assert!(!v("00000000-0000-4000-8000-000000000001\n"));
}

#[test]
fn s_cache_corrupt_json_is_ignored_and_rewritten() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    fs::write(env.sdir.join(format!("{}.jsonl", uid(1))), user_line("2024-05-01T10:00:00Z", &cwd, "oi")).unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = cache_dir.path().join("sub").join("idx.json");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    for garbage in [&b"{{{{ not json"[..], b"", b"null", br#"{"version":1,"entries":{"x":42}}"#, br#"{"version":999,"entries":{}}"#] {
        fs::write(&cache, garbage).unwrap();
        let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
        assert_eq!(r.sessions.len(), 1, "cache {garbage:?}");
        let again: Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        assert_eq!(again["version"], 1);
    }
}

#[test]
fn s_cache_unwritable_location_does_not_break_listing() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    fs::write(env.sdir.join(format!("{}.jsonl", uid(1))), user_line("2024-05-01T10:00:00Z", &cwd, "oi")).unwrap();
    let blocker = env.proj.join("file");
    fs::write(&blocker, "x").unwrap();
    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&blocker.join("idx.json"))).unwrap();
    assert_eq!(r.sessions.len(), 1);
}

#[test]
fn s_cache_same_mtime_different_size_reparses() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let f = env.sdir.join(format!("{}.jsonl", uid(1)));
    let l1 = user_line("2024-05-01T10:00:00Z", &cwd, "um");
    fs::write(&f, &l1).unwrap();
    let mt = fs::metadata(&f).unwrap().modified().unwrap();
    let cache = env.cp.join("idx.json");
    let r1 = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    assert_eq!(r1.sessions[0].message_count, 1);
    let l2 = user_line("2024-05-01T10:00:01Z", &cwd, "dois");
    fs::write(&f, format!("{l1}\n{l2}")).unwrap();
    File::options().write(true).open(&f).unwrap().set_modified(mt).unwrap();
    let r2 = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    assert_eq!(r2.sessions[0].message_count, 2, "cache não invalidou com size diferente e mtime igual");
}

#[test]
fn s_cache_edit_between_listings_with_new_mtime() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let f = env.sdir.join(format!("{}.jsonl", uid(1)));
    fs::write(&f, user_line("2024-05-01T10:00:00Z", &cwd, "um")).unwrap();
    let cache = env.cp.join("idx.json");
    let _ = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    // mesmo tamanho, mtime diferente (+5s)
    fs::write(&f, user_line("2024-05-01T10:00:00Z", &cwd, "UM")).unwrap();
    File::options().write(true).open(&f).unwrap().set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    assert_eq!(r.sessions[0].title, "UM");
}

#[test]
fn s_cache_same_size_same_mtime_is_stale_characterization() {
    // Limitação conhecida da heurística (mtime,size): documentar, não é falha.
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let f = env.sdir.join(format!("{}.jsonl", uid(1)));
    fs::write(&f, user_line("2024-05-01T10:00:00Z", &cwd, "um")).unwrap();
    let mt = fs::metadata(&f).unwrap().modified().unwrap();
    let cache = env.cp.join("idx.json");
    let _ = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    fs::write(&f, user_line("2024-05-01T10:00:00Z", &cwd, "UM")).unwrap();
    File::options().write(true).open(&f).unwrap().set_modified(mt).unwrap();
    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    eprintln!("[OBS] mesmo size+mtime -> título='{}' (stale se 'um')", r.sessions[0].title);
}

#[test]
fn s_active_flag_recomputed_from_cache() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let f = env.sdir.join(format!("{}.jsonl", uid(1)));
    fs::write(&f, user_line("2024-05-01T10:00:00Z", &cwd, "um")).unwrap();
    let cache = env.cp.join("idx.json");
    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    assert!(r.sessions[0].active);
    // envelhece o arquivo 1h: reparse (mtime muda) e active=false
    File::options().write(true).open(&f).unwrap().set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
    assert!(!r.sessions[0].active);
}

#[test]
fn s_cache_concurrent_savers_never_leave_corrupt_index() {
    // Dois listagens simultâneas (ex.: get + scan) escrevem o mesmo idx.json via mesmo .tmp.
    let env = Arc::new(env_with_project("p"));
    let cwd = env.proj.to_string_lossy().into_owned();
    for i in 0..200 {
        fs::write(env.sdir.join(format!("{}.jsonl", uid(i + 1))), user_line("2024-05-01T10:00:00Z", &cwd, &format!("t{i}"))).unwrap();
    }
    let cache = Arc::new(env.cp.join("idx.json"));
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicUsize::new(0));
    let reader = {
        let (cache, stop, bad) = (cache.clone(), stop.clone(), bad.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(b) = fs::read(&*cache)
                    && serde_json::from_slice::<Value>(&b).is_err()
                {
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let ws: Vec<_> = (0..4)
        .map(|_| {
            let (env, cache) = (env.clone(), cache.clone());
            std::thread::spawn(move || {
                for _ in 0..40 {
                    let _ = fs::remove_file(&*cache); // força dirty
                    let r = sessions::list_sessions_cached(&env.cp, &env.proj, Some(&cache)).unwrap();
                    assert_eq!(r.sessions.len(), 200);
                }
            })
        })
        .collect();
    for w in ws {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    assert_eq!(bad.load(Ordering::Relaxed), 0, "leitor viu índice de cache parcial/corrompido");
}

#[test]
fn s_unreadable_and_degraded_counts() {
    let env = env_with_project("p");
    let cwd = env.proj.to_string_lossy().into_owned();
    let good = user_line("2024-05-01T10:00:00Z", &cwd, "ok");
    fs::write(env.sdir.join(format!("{}.jsonl", uid(1))), format!("{good}\n{{broken")).unwrap();
    fs::write(env.sdir.join(format!("{}.jsonl", uid(2))), "").unwrap();
    let r = list(&env);
    assert_eq!(r.degraded_count, 1);
    assert_eq!(r.unreadable.len(), 1);
    assert!(UnreadableSession { file_name: "x".into(), reason: "y".into() } == UnreadableSession { file_name: "x".into(), reason: "y".into() });
}

// ============================================================ LAUNCHER

fn tcfg(kind: TerminalKind) -> TerminalConfig {
    TerminalConfig { kind, ..Default::default() }
}

fn exec_file(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const HOSTILE_DIRS: &[&str] = &[
    "-rf",
    "--help",
    "a b",
    "a\"b'c",
    "a;touch PWNED",
    "$(touch PWNED)",
    "`touch PWNED`",
    "a\nb",
    "ü日本😀",
    "a&&b|c>d",
    "{command}",
    "{dir}",
];

#[test]
fn l_build_command_hostile_dirs_all_variants_are_single_argv() {
    let claude = Path::new("/usr/bin/claude");
    for name in HOSTILE_DIRS {
        let dir = PathBuf::from("/tmp").join(name);
        for kind in [TerminalKind::Ptyxis, TerminalKind::GnomeTerminal] {
            let spec = launcher::build_command(&tcfg(kind), claude, &dir, &LaunchMode::New).unwrap();
            let want = format!("--working-directory={}", dir.display());
            assert!(spec.args.iter().any(|a| a.to_string_lossy() == want), "{name:?}: {:?}", spec.args);
            let dashdash = spec.args.iter().position(|a| a == "--").unwrap();
            assert_eq!(spec.args[dashdash + 1], claude.as_os_str());
            assert_eq!(spec.args.len(), dashdash + 2);
        }
    }
    // relativo começando com '-' direto no build_command (não ocorre via launch_project_with_bases: lá é canônico)
    let spec = launcher::build_command(&tcfg(TerminalKind::Ptyxis), claude, Path::new("-x"), &LaunchMode::New).unwrap();
    assert!(spec.args.iter().any(|a| a == "--working-directory=-x"));
}

#[test]
fn l_build_command_never_uses_shell() {
    let claude = Path::new("/usr/bin/claude");
    for kind in [TerminalKind::Ptyxis, TerminalKind::GnomeTerminal] {
        let spec = launcher::build_command(&tcfg(kind), claude, Path::new("/tmp/x"), &LaunchMode::Continue).unwrap();
        assert!(!matches!(spec.program.to_str(), Some("sh" | "bash" | "/bin/sh" | "/bin/bash")));
        assert!(!spec.args.iter().any(|a| a == "-c" || a == "bash" || a == "sh"));
        assert_eq!(spec.args.last().unwrap(), "--continue");
    }
}

#[test]
fn l_malicious_session_ids_rejected_before_argv() {
    let claude = Path::new("/usr/bin/claude");
    let bad = [
        "--help",
        "--dangerously-skip-permissions",
        "x --dangerously-skip-permissions",
        "../..",
        "",
        "00000000-0000-4000-8000-000000000001 --dangerously-skip-permissions",
        "00000000-0000-4000-8000-00000000000\0",
        "00000000-0000-4000-8000-000000000001\n--flag",
    ];
    let td = tempfile::tempdir().unwrap();
    let known = vec![td.path().to_string_lossy().into_owned()];
    for id in bad {
        let mode = LaunchMode::Resume { session_id: id.to_string() };
        for kind in [TerminalKind::Ptyxis, TerminalKind::GnomeTerminal] {
            let r = launcher::build_command(&tcfg(kind), claude, Path::new("/tmp/x"), &mode);
            assert!(matches!(r, Err(LaunchError::InvalidSessionId)), "build_command aceitou {id:?}");
        }
        let c = TerminalConfig {
            kind: TerminalKind::Custom,
            custom_executable: Some("/bin/true".into()),
            custom_args: vec!["{command}".into()],
            claude_path: None,
        };
        assert!(matches!(launcher::build_command(&c, claude, Path::new("/tmp/x"), &mode), Err(LaunchError::InvalidSessionId)));
        assert!(matches!(launcher::validate_launch(td.path(), &mode, &known), Err(LaunchError::InvalidSessionId)), "validate_launch aceitou {id:?}");
    }
}

#[test]
fn l_resume_valid_id_argv() {
    let id = "ABCDEF01-0000-4000-8000-00000000000A";
    let spec = launcher::build_command(
        &tcfg(TerminalKind::Ptyxis),
        Path::new("/usr/bin/claude"),
        Path::new("/tmp/x"),
        &LaunchMode::Resume { session_id: id.into() },
    )
    .unwrap();
    let n = spec.args.len();
    assert_eq!(spec.args[n - 2], "--resume");
    assert_eq!(spec.args[n - 1], id);
}

fn custom(exe: Option<&str>, args: &[&str]) -> TerminalConfig {
    TerminalConfig {
        kind: TerminalKind::Custom,
        custom_executable: exe.map(String::from),
        custom_args: args.iter().map(|s| s.to_string()).collect(),
        claude_path: None,
    }
}

#[test]
fn l_custom_args_placeholders() {
    let claude = Path::new("/usr/bin/claude");
    let dir = Path::new("/tmp/my dir");
    let mode = LaunchMode::Continue;
    // {dir} repetido, embutido
    let s = launcher::build_command(&custom(Some("/bin/term"), &["--cwd={dir}", "{dir}", "x{dir}y{dir}z", "{command}"]), claude, dir, &mode).unwrap();
    let a: Vec<String> = s.args.iter().map(|x| x.to_string_lossy().into_owned()).collect();
    assert_eq!(a, ["--cwd=/tmp/my dir", "/tmp/my dir", "x/tmp/my diry/tmp/my dirz", "/usr/bin/claude", "--continue"]);
    // sem {dir}
    let s = launcher::build_command(&custom(Some("/bin/term"), &["{command}"]), claude, dir, &mode).unwrap();
    assert_eq!(s.args.len(), 2);
    // {command} duplicado -> expandido duas vezes
    let s = launcher::build_command(&custom(Some("/bin/term"), &["{command}", "--", "{command}"]), claude, dir, &mode).unwrap();
    assert_eq!(s.args.len(), 5);
    // sem {command}
    assert!(matches!(launcher::build_command(&custom(Some("/bin/term"), &["{dir}"]), claude, dir, &mode), Err(LaunchError::CustomMissingCommand)));
    // {command} só embutido -> erro
    assert!(matches!(launcher::build_command(&custom(Some("/bin/term"), &["--c={command}"]), claude, dir, &mode), Err(LaunchError::CustomMissingCommand)));
    // sem executável / só espaços
    for e in [None, Some(""), Some("   ")] {
        assert!(matches!(launcher::build_command(&custom(e, &["{command}"]), claude, dir, &mode), Err(LaunchError::CustomNotConfigured)));
    }
}

#[test]
fn l_custom_embedded_command_is_literal_when_exact_also_present() {
    // Observação: "--x={command}" junto com "{command}" fica literal (não expandido), silenciosamente.
    let s = launcher::build_command(
        &custom(Some("/bin/term"), &["--x={command}", "{command}"]),
        Path::new("/usr/bin/claude"),
        Path::new("/tmp/d"),
        &LaunchMode::New,
    )
    .unwrap();
    eprintln!("[OBS] args={:?}", s.args);
    assert_eq!(s.args[0], "--x={command}");
}

#[test]
fn l_custom_dir_containing_placeholder_not_reexpanded() {
    let s = launcher::build_command(
        &custom(Some("/bin/term"), &["{dir}", "{command}"]),
        Path::new("/usr/bin/claude"),
        Path::new("/tmp/{command}{dir}"),
        &LaunchMode::New,
    )
    .unwrap();
    assert_eq!(s.args[0], "/tmp/{command}{dir}");
    assert_eq!(s.args.len(), 2);
}

#[test]
fn l_resolve_claude_paths() {
    let td = tempfile::tempdir().unwrap();
    let with = |p: &Path| TerminalConfig { claude_path: Some(p.to_string_lossy().into_owned()), ..Default::default() };
    let ok = exec_file(td.path(), "claude", "#!/bin/sh\nexit 0\n");
    assert_eq!(launcher::resolve_claude(&with(&ok)).unwrap(), ok);
    // inexistente
    assert!(matches!(launcher::resolve_claude(&with(&td.path().join("nope"))), Err(LaunchError::ClaudeNotFound(_))));
    // diretório
    let d = td.path().join("adir");
    fs::create_dir(&d).unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(launcher::resolve_claude(&with(&d)), Err(LaunchError::ClaudeNotFound(_))));
    // sem bit de execução
    let nx = td.path().join("noexec");
    fs::write(&nx, "x").unwrap();
    fs::set_permissions(&nx, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(launcher::resolve_claude(&with(&nx)), Err(LaunchError::ClaudeNotFound(_))));
    // symlink para executável: aceito
    let ln = td.path().join("ln");
    symlink(&ok, &ln).unwrap();
    assert_eq!(launcher::resolve_claude(&with(&ln)).unwrap(), ln);
    // symlink quebrado
    let br = td.path().join("broken");
    symlink(td.path().join("zzz"), &br).unwrap();
    assert!(matches!(launcher::resolve_claude(&with(&br)), Err(LaunchError::ClaudeNotFound(_))));
}

#[test]
fn l_resolve_claude_relative_path_is_absolutized() {
    // spawn faz chdir(projeto) antes do exec; caminho relativo passaria a apontar para o lugar errado.
    let td = tempfile::tempdir().unwrap();
    let ok = exec_file(td.path(), "claude", "#!/bin/sh\nexit 0\n");
    let cwd = std::env::current_dir().unwrap();
    let up = "../".repeat(cwd.components().count() - 1);
    let rel = format!("{up}{}", ok.strip_prefix("/").unwrap().display());
    assert!(Path::new(&rel).is_relative() && Path::new(&rel).exists());
    let cfg = TerminalConfig { claude_path: Some(rel), ..Default::default() };
    // recusar (Err) também é aceitável
    if let Ok(p) = launcher::resolve_claude(&cfg) {
        assert!(p.is_absolute(), "claude_path relativo devolvido como {p:?}; quebra após current_dir(projeto)");
    }
}

fn known(p: &Path) -> Vec<String> {
    vec![p.to_string_lossy().into_owned()]
}

#[test]
fn l_validate_launch_dir_cases() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let a = root.join("a");
    let b = root.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let canon_b = fs::canonicalize(&b).unwrap();
    let new = LaunchMode::New;
    // conhecido: ok e canônico
    assert_eq!(launcher::validate_launch(&b, &new, &known(&b)).unwrap(), canon_b);
    // não canônico a/../b
    assert_eq!(launcher::validate_launch(&a.join("../b"), &new, &known(&b)).unwrap(), canon_b);
    // conhecido não canônico
    assert_eq!(launcher::validate_launch(&b, &new, &known(&a.join("../b"))).unwrap(), canon_b);
    // fora dos conhecidos
    assert!(matches!(launcher::validate_launch(&a, &new, &known(&b)), Err(LaunchError::UnknownProject(_))));
    assert!(matches!(launcher::validate_launch(&a, &new, &[]), Err(LaunchError::UnknownProject(_))));
    // subdiretório de projeto conhecido não é conhecido
    let sub = b.join("sub");
    fs::create_dir(&sub).unwrap();
    assert!(matches!(launcher::validate_launch(&sub, &new, &known(&b)), Err(LaunchError::UnknownProject(_))));
    // ancestral
    assert!(matches!(launcher::validate_launch(root, &new, &known(&b)), Err(LaunchError::UnknownProject(_))));
    // vazio / inexistente / arquivo
    assert!(matches!(launcher::validate_launch(Path::new(""), &new, &known(&b)), Err(LaunchError::InvalidDir(_))));
    assert!(matches!(launcher::validate_launch(&root.join("zz"), &new, &known(&b)), Err(LaunchError::InvalidDir(_))));
    let f = root.join("file");
    fs::write(&f, "x").unwrap();
    assert!(matches!(launcher::validate_launch(&f, &new, &known(&f)), Err(LaunchError::InvalidDir(_))));
    // prefixo de nome: /a vs /ab
    let ab = root.join("ab");
    fs::create_dir(&ab).unwrap();
    assert!(matches!(launcher::validate_launch(&ab, &new, &known(&a)), Err(LaunchError::UnknownProject(_))));
}

#[test]
fn l_validate_launch_symlinks() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let proj = root.join("proj");
    let outside = root.join("outside");
    fs::create_dir_all(&proj).unwrap();
    fs::create_dir_all(&outside).unwrap();
    // symlink dentro do projeto para fora: não vira conhecido
    let evil = proj.join("evil");
    symlink(&outside, &evil).unwrap();
    let new = LaunchMode::New;
    assert!(matches!(launcher::validate_launch(&evil, &new, &known(&proj)), Err(LaunchError::UnknownProject(_))));
    // symlink para o projeto conhecido: devolve o canônico do projeto
    let ln = root.join("ln");
    symlink(&proj, &ln).unwrap();
    assert_eq!(launcher::validate_launch(&ln, &new, &known(&proj)).unwrap(), fs::canonicalize(&proj).unwrap());
    // conhecido que é symlink para outside; dir outside => aceito (confia em known)
    assert!(launcher::validate_launch(&outside, &new, &[evil.to_string_lossy().into_owned()]).is_ok());
    // troca de alvo (TOCTOU simplificado): symlink retargetado após o "scan"
    let swap = root.join("swap");
    symlink(&proj, &swap).unwrap();
    let k = known(&swap); // known aponta p/ symlink -> proj
    fs::remove_file(&swap).unwrap();
    symlink(&outside, &swap).unwrap(); // agora aponta p/ fora
    let r = launcher::validate_launch(&swap, &new, &k);
    eprintln!("[OBS] known=symlink retargetado para fora -> {r:?}");
    // known e dir são o mesmo symlink; ambos resolvem p/ outside, então é aceito (comparação canônica x canônica).
}

#[test]
fn l_validate_launch_dir_removed_between_scan_and_launch() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("gone");
    fs::create_dir(&p).unwrap();
    let k = known(&p);
    fs::remove_dir(&p).unwrap();
    assert!(matches!(launcher::validate_launch(&p, &LaunchMode::New, &k), Err(LaunchError::InvalidDir(_))));
}

#[test]
fn l_launch_project_e2e_hostile_dir_goes_through_no_shell() {
    // UM único teste forka processos (evita ETXTBSY e fallback p/ ptyxis real).
    let td = tempfile::tempdir().unwrap();
    let out = td.path().join("argv.out");
    let term = exec_file(
        td.path(),
        "fake-term",
        &format!("#!/bin/sh\nprintf '%s\\0' \"$@\" > '{o}.tmp'\nmv '{o}.tmp' '{o}'\n", o = out.display()),
    );
    let claude = exec_file(td.path(), "claude", "#!/bin/sh\nexit 0\n");
    let cfg = TerminalConfig {
        kind: TerminalKind::Custom,
        custom_executable: Some(term.to_string_lossy().into_owned()),
        custom_args: vec!["--cwd".into(), "{dir}".into(), "--".into(), "{command}".into()],
        claude_path: Some(claude.to_string_lossy().into_owned()),
    };
    let lcfg = Config { terminal: cfg, ..Config::default() };
    let hostile = td.path().join("a b;touch PWNED $(touch PWNED2) `touch PWNED3` \"q\" 'x' ü\nnl -rf");
    fs::create_dir(&hostile).unwrap();
    let id = "00000000-0000-4000-8000-0000000000aa";
    let k = known(&hostile);

    // id malicioso: nada é lançado
    let bad = launcher::launch_project_with_bases(&hostile, LaunchMode::Resume { session_id: "--dangerously-skip-permissions".into() }, &k, &lcfg);
    assert!(matches!(bad, Err(LaunchError::InvalidSessionId)));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!out.exists(), "algo foi lançado com id inválido");

    // projeto desconhecido
    let unk = launcher::launch_project_with_bases(&hostile, LaunchMode::New, &[], &lcfg);
    assert!(matches!(unk, Err(LaunchError::UnknownProject(_))));
    assert!(!out.exists());

    let outcome = launcher::launch_project_with_bases(&hostile, LaunchMode::Resume { session_id: id.into() }, &k, &lcfg).unwrap();
    assert_eq!(outcome.terminal_used, TerminalKind::Custom);
    let t = Instant::now();
    while !out.exists() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(out.exists(), "terminal falso não gravou argv (possível fallback para terminal real!)");
    let raw = fs::read(&out).unwrap();
    let args: Vec<String> = raw.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let canon = fs::canonicalize(&hostile).unwrap().to_string_lossy().into_owned();
    assert_eq!(args, vec!["--cwd".to_string(), canon, "--".into(), claude.to_string_lossy().into_owned(), "--resume".into(), id.into()]);
    for name in ["PWNED", "PWNED2", "PWNED3"] {
        for base in [td.path().to_path_buf(), hostile.clone(), std::env::current_dir().unwrap()] {
            assert!(!base.join(name).exists(), "{name} criado em {base:?}: injeção de shell");
        }
    }
}

// ============================================================ CONFIG

fn cfg_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn baks(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.bak"))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), fs::read(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

#[test]
fn c_three_consecutive_corruptions_keep_all_backups() {
    let td = cfg_dir();
    let f = td.path().join("config.json");
    for (i, body) in ["{corrupt-1", "{corrupt-2", "{corrupt-3"].iter().enumerate() {
        fs::write(&f, body).unwrap();
        let l = Config::load_from(td.path()).unwrap();
        assert!(l.recovered, "iteração {i}");
        assert_eq!(l.config, Config::default());
    }
    let b = baks(td.path());
    let bodies: BTreeSet<_> = b.iter().map(|(_, c)| String::from_utf8_lossy(c).into_owned()).collect();
    assert_eq!(bodies, BTreeSet::from(["{corrupt-1".into(), "{corrupt-2".into(), "{corrupt-3".into()]), "backups: {b:?}");
    // Mudou (item 5): o load regrava os padrões; o corrompido está só nos backups.
    assert!(!Config::load_from(td.path()).unwrap().recovered);
}

#[test]
fn c_repeated_loads_of_same_corruption_accumulate_backups() {
    // Observação: cada load() de um config corrompido cria outro .bak (get_config + scan_projects = 2 por ação).
    let td = cfg_dir();
    fs::write(td.path().join("config.json"), "{corrupt").unwrap();
    // Mudou (item 5): só a 1ª carga recupera; os padrões são regravados e as seguintes são limpas.
    assert!(Config::load_from(td.path()).unwrap().recovered);
    for _ in 0..4 {
        assert!(!Config::load_from(td.path()).unwrap().recovered);
    }
    let n = baks(td.path()).len();
    eprintln!("[OBS] 5 loads do mesmo arquivo corrompido -> {n} arquivos .bak");
    assert!(n <= 1, "{n} backups idênticos acumulados; load deveria ser idempotente ou o original deveria ser movido");
}

#[test]
fn c_empty_and_shape_variants() {
    let td = cfg_dir();
    let f = td.path().join("config.json");
    let load = |body: &str| {
        fs::write(&f, body).unwrap();
        Config::load_from(td.path()).unwrap()
    };
    let l = load("{}");
    assert!(!l.recovered);
    assert_eq!(l.config, Config::default());
    let l = load(r#"{"maxDepth":3,"campoDesconhecido":{"a":1},"terminal":{"extra":true}}"#);
    assert!(!l.recovered);
    assert_eq!(l.config.max_depth, 3);
    assert_eq!(l.config.regions.len(), 6);
    assert!(load("").recovered, "arquivo vazio deveria ser tratado como corrompido");
    assert!(load("null").recovered);
    assert!(load(r#"{"maxDepth":"6"}"#).recovered);
    assert!(load(r#"{"maxDepth":-1}"#).recovered);
    assert!(load(r#"{"roots":"/x"}"#).recovered);
    // BOM
    let l = load("\u{feff}{}");
    eprintln!("[OBS] config com BOM -> recovered={}", l.recovered);
}

#[test]
fn c_json_array_is_not_a_valid_config() {
    // serde derive aceita sequência posicional: "[]" vira Config padrão sem recovered/backup.
    let td = cfg_dir();
    fs::write(td.path().join("config.json"), "[]").unwrap();
    let l = Config::load_from(td.path()).unwrap();
    assert!(l.recovered, "'[]' foi aceito como config válida (recovered=false)");
}

#[test]
fn c_load_does_not_validate_loaded_values() {
    // Observação: load aceita maxDepth 0 / ids duplicados; só save valida.
    let td = cfg_dir();
    fs::write(td.path().join("config.json"), r#"{"maxDepth":0,"regions":[{"id":"a"},{"id":"a","color":"red"}]}"#).unwrap();
    let l = Config::load_from(td.path()).unwrap();
    eprintln!("[OBS] load: recovered={} maxDepth={} validate={:?}", l.recovered, l.config.max_depth, l.config.validate().is_err());
    assert!(!l.recovered);
    assert!(l.config.validate().is_err());
}

#[test]
fn c_validate_colors_ids_depth() {
    let mk = |regions: Vec<Region>, depth: u32| Config { regions, max_depth: depth, ..Config::default() };
    let r = |id: &str, color: &str| Region { id: id.into(), name: "n".into(), color: color.into(), description: "".into(), ..Default::default() };
    for bad in ["#GGGGGG", "#FFF", "red", "", "#12345", "#1234567", "123456", "#12345G", " #123456", "#１２３４５６", "#ff00ff\n"] {
        assert!(mk(vec![r("a", bad)], 6).validate().is_err(), "cor {bad:?} aceita");
    }
    for ok in ["#000000", "#abcdef", "#ABCDEF", "#aBc123"] {
        assert!(mk(vec![r("a", ok)], 6).validate().is_ok(), "cor {ok:?} rejeitada");
    }
    assert!(mk(vec![r("a", "#000000"), r("a", "#111111")], 6).validate().is_err(), "ids duplicados");
    assert!(mk(vec![r("", "#000000")], 6).validate().is_err());
    assert!(mk(vec![r("   ", "#000000")], 6).validate().is_err());
    assert!(mk(vec![], 6).validate().is_err(), "regiões vazias");
    for d in [0, 13, u32::MAX] {
        assert!(mk(vec![r("a", "#000000")], d).validate().is_err(), "maxDepth {d}");
    }
    for d in [1, 12] {
        assert!(mk(vec![r("a", "#000000")], d).validate().is_ok());
    }
    // ids que diferem só por caixa / espaços
    assert!(mk(vec![r("A", "#000000"), r("a", "#000000")], 6).validate().is_ok());
    let c = mk(vec![r("a", "#000000"), r("a ", "#000000")], 6);
    eprintln!("[OBS] ids 'a' e 'a ' (espaço final) -> validate ok={}", c.validate().is_ok());
}

#[test]
fn c_project_referencing_unknown_region_is_rejected_on_save() {
    let td = cfg_dir();
    let mut c = Config::default();
    c.projects.insert("/x".into(), ProjectMeta { region: Some("nao-existe".into()), ..Default::default() });
    let r = c.save_to(td.path());
    eprintln!("[OBS] save com projeto -> região inexistente: ok={}", r.is_ok());
    assert!(r.is_err(), "save aceitou projeto com região inexistente (scan faz fallback silencioso)");
}

#[test]
fn c_save_roundtrip_and_permissions() {
    let td = cfg_dir();
    let dir = td.path().join("novo/aninhado");
    let mut c = Config::default();
    c.projects.insert("/x/ü".into(), ProjectMeta { tags: vec!["t".into()], display_name: Some("Nome".into()), ..Default::default() });
    c.save_to(&dir).unwrap();
    let back = Config::load_from(&dir).unwrap();
    assert!(!back.recovered);
    assert_eq!(back.config, c);
    let mode = fs::metadata(dir.join("config.json")).unwrap().permissions().mode() & 0o777;
    eprintln!("[OBS] modo de config.json = {mode:o}");
    assert_eq!(mode & 0o002, 0, "config.json gravável por outros");
    assert!(!dir.join("config.json.tmp").exists(), "tmp residual");
}

#[test]
fn c_save_failure_cases_do_not_touch_existing_file() {
    let td = cfg_dir();
    let dir = td.path().join("d");
    Config::default().save_to(&dir).unwrap();
    let before = fs::read(dir.join("config.json")).unwrap();
    // diretório sem permissão de escrita
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let mut c = Config { max_depth: 9, ..Config::default() };
    let r = c.save_to(&dir);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(r, Err(ConfigError::Io { .. })), "{r:?}");
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
    // config inválida não escreve nada
    c.max_depth = 99;
    assert!(matches!(c.save_to(&dir), Err(ConfigError::Invalid(_))));
    assert_eq!(fs::read(dir.join("config.json")).unwrap(), before);
    // dir é um arquivo
    let f = td.path().join("afile");
    fs::write(&f, "x").unwrap();
    assert!(Config::default().save_to(&f).is_err());
    // Mudou (item 7): o temporário tem nome único; um config.json.tmp pré-existente não atrapalha.
    let d2 = td.path().join("d2");
    fs::create_dir_all(d2.join("config.json.tmp")).unwrap();
    assert!(Config::default().save_to(&d2).is_ok());
}

fn big_config(depth: u32, tag: &str) -> Config {
    let mut c = Config { max_depth: depth, ..Config::default() };
    for i in 0..3000 {
        c.projects.insert(format!("/w/{tag}/proj-{i}"), ProjectMeta { tags: vec![tag.repeat(20)], display_name: Some(tag.repeat(30)), ..Default::default() });
    }
    c
}

#[test]
fn c_concurrent_saves_never_publish_partial_file() {
    let td = cfg_dir();
    let dir = Arc::new(td.path().to_path_buf());
    let (ca, cb) = (Arc::new(big_config(3, "a")), Arc::new(big_config(9, "b")));
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    ca.save_to(&dir).unwrap();
    let reader = {
        let (dir, stop, bad, reads) = (dir.clone(), stop.clone(), bad.clone(), reads.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(b) = fs::read(dir.join("config.json")) {
                    reads.fetch_add(1, Ordering::Relaxed);
                    if serde_json::from_slice::<Config>(&b).is_err() {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
    };
    let errs = Arc::new(AtomicUsize::new(0));
    let ws: Vec<_> = [ca.clone(), cb.clone()]
        .into_iter()
        .map(|c| {
            let (dir, errs) = (dir.clone(), errs.clone());
            std::thread::spawn(move || {
                for _ in 0..150 {
                    if c.save_to(&dir).is_err() {
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for w in ws {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    let fin: Config = serde_json::from_slice(&fs::read(dir.join("config.json")).unwrap()).expect("arquivo final corrompido");
    assert!(fin == *ca || fin == *cb, "arquivo final é mistura das duas escritas");
    eprintln!("[OBS] leituras={} parciais={} saves_com_erro={}", reads.load(Ordering::Relaxed), bad.load(Ordering::Relaxed), errs.load(Ordering::Relaxed));
    assert_eq!(bad.load(Ordering::Relaxed), 0, "leitor viu config.json parcial/corrompido");
    assert_eq!(errs.load(Ordering::Relaxed), 0, "saves concorrentes falharam (tmp compartilhado)");
}

// ============================================================ GIT

fn head_of(content: &[u8]) -> Option<GitHead> {
    let td = tempfile::tempdir().unwrap();
    fs::create_dir(td.path().join(".git")).unwrap();
    fs::write(td.path().join(".git/HEAD"), content).unwrap();
    git::read_branch(td.path())
}

#[test]
fn g_head_variants() {
    assert_eq!(head_of(b""), None);
    assert_eq!(head_of(b"\n  \n"), None);
    assert_eq!(head_of(b"ref:"), None);
    assert_eq!(head_of(b"ref: \n"), None);
    assert_eq!(head_of(b"ref: refs/heads/\n"), None);
    assert_eq!(head_of(b"ref: refs/heads/main\n"), Some(GitHead::Branch("main".into())));
    assert_eq!(head_of(b"ref: refs/heads/main\r\n"), Some(GitHead::Branch("main".into())));
    assert_eq!(head_of(b"ref:refs/heads/feat/x y\n"), Some(GitHead::Branch("feat/x y".into())));
    assert_eq!(head_of("ref: refs/heads/ü-日本\n".as_bytes()), Some(GitHead::Branch("ü-日本".into())));
    assert_eq!(head_of(b"ref: refs/tags/v1\n"), Some(GitHead::Branch("refs/tags/v1".into())));
    assert_eq!(head_of(b"0123456789abcdef0123456789abcdef01234567\n"), Some(GitHead::Detached("0123456".into())));
    assert_eq!(head_of(&b"ab".repeat(32)), Some(GitHead::Detached("abababa".into()))); // sha256
    assert_eq!(head_of(b"abc\n"), None);
    assert_eq!(head_of(b"abcdef\n"), None);
    assert_eq!(head_of(b"0123456789abcdeg0123456789abcdef01234567\n"), None);
    assert_eq!(head_of(b"\xff\xfe\xfd"), None);
    assert_eq!(head_of(b"ref: refs/heads/a\0b\n"), Some(GitHead::Branch("a\0b".into())));
}

#[test]
fn g_short_hex_is_not_a_hash() {
    // Hash inválido: 7..39 hex não é um SHA válido (40 ou 64).
    assert_eq!(head_of(b"deadbee\n"), None, "7 hex aceito como hash");
    assert_eq!(head_of(&b"a".repeat(41)), None, "41 hex aceito como hash");
}

#[test]
fn g_gitdir_file_variants() {
    let td = tempfile::tempdir().unwrap();
    let real = td.path().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
    let mk = |name: &str, content: &str| {
        let p = td.path().join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join(".git"), content).unwrap();
        p
    };
    // relativo
    assert_eq!(git::read_branch(&mk("rel", "gitdir: ../real\n")), Some(GitHead::Branch("wt".into())));
    // absoluto
    assert_eq!(git::read_branch(&mk("abs", &format!("gitdir: {}\n", real.display()))), Some(GitHead::Branch("wt".into())));
    // inexistente / vazio / sem prefixo / primeira linha em branco
    assert_eq!(git::read_branch(&mk("nx", "gitdir: /nope/nope\n")), None);
    assert_eq!(git::read_branch(&mk("empty", "gitdir:\n")), None);
    assert_eq!(git::read_branch(&mk("nopref", "../real\n")), None);
    assert_eq!(git::read_branch(&mk("blank", "\ngitdir: ../real\n")), None);
    assert_eq!(git::read_branch(&mk("zero", "")), None);
    // circular: aponta para si mesmo / para "." / para o próprio arquivo
    assert_eq!(git::read_branch(&mk("selfref", "gitdir: .git\n")), None);
    assert_eq!(git::read_branch(&mk("dot", "gitdir: .\n")), None);
    let cyc = mk("cyc", "gitdir: ../cyc2\n");
    let c2 = td.path().join("cyc2");
    fs::create_dir_all(&c2).unwrap();
    fs::write(c2.join(".git"), "gitdir: ../cyc\n").unwrap();
    assert_eq!(git::read_branch(&cyc), None);
    // sem .git
    assert_eq!(git::read_branch(&td.path().join("naoexiste")), None);
}

#[test]
fn g_huge_head_bounded() {
    let td = tempfile::tempdir().unwrap();
    fs::create_dir(td.path().join(".git")).unwrap();
    let f = td.path().join(".git/HEAD");
    let mut h = File::create(&f).unwrap();
    let chunk = vec![b'z'; 1 << 20];
    for _ in 0..200 {
        h.write_all(&chunk).unwrap();
    }
    drop(h);
    reset_hwm();
    let rss0 = proc_kb("VmRSS:");
    let t = Instant::now();
    let r = git::read_branch(td.path());
    let el = t.elapsed();
    let delta = proc_kb("VmHWM:").saturating_sub(rss0);
    eprintln!("[VOLUME] HEAD de 200 MiB: {el:?}, delta RSS pico={delta} kB, resultado={r:?}");
    assert_eq!(r, None);
    assert!(delta < 64 * 1024, "read_branch carregou o HEAD inteiro na memória ({delta} kB)");
}

// ============================================================ SERDE / CONTRATO COM O FRONT

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn set(a: &[&str]) -> BTreeSet<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn j_launch_mode_roundtrip() {
    assert_eq!(serde_json::to_value(LaunchMode::New).unwrap(), json!({"kind":"new"}));
    assert_eq!(serde_json::to_value(LaunchMode::Continue).unwrap(), json!({"kind":"continue"}));
    assert_eq!(serde_json::to_value(LaunchMode::Resume { session_id: "abc".into() }).unwrap(), json!({"kind":"resume","sessionId":"abc"}));
    assert_eq!(serde_json::from_value::<LaunchMode>(json!({"kind":"resume","sessionId":"abc"})).unwrap(), LaunchMode::Resume { session_id: "abc".into() });
    assert_eq!(serde_json::from_value::<LaunchMode>(json!({"kind":"new"})).unwrap(), LaunchMode::New);
    assert!(serde_json::from_value::<LaunchMode>(json!({"kind":"resume"})).is_err());
    assert!(serde_json::from_value::<LaunchMode>(json!({"kind":"resume","session_id":"abc"})).is_err(), "snake_case aceito");
    assert!(serde_json::from_value::<LaunchMode>(json!({"kind":"rm -rf"})).is_err());
    assert!(serde_json::from_value::<LaunchMode>(json!("new")).is_err());
}

#[test]
fn j_config_shape() {
    let v = serde_json::to_value(Config::default()).unwrap();
    assert_eq!(keys(&v), set(&["roots", "maxDepth", "treeMaxDepth", "ignore", "showOrphans", "terminal", "regions", "projects", "bases"]));
    assert_eq!(keys(&v["terminal"]), set(&["kind", "customExecutable", "customArgs", "claudePath"]));
    assert_eq!(keys(&v["regions"][0]), set(&["id", "name", "color", "description", "kind", "access"]));
    assert_eq!(v["terminal"]["kind"], "ptyxis");
    let mut c = Config::default();
    c.projects.insert("/p".into(), ProjectMeta::default());
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(keys(&v["projects"]["/p"]), set(&["region", "tags", "dependsOn", "hidden", "displayName"]));
    for (k, s) in [(TerminalKind::Ptyxis, "ptyxis"), (TerminalKind::GnomeTerminal, "gnomeTerminal"), (TerminalKind::Custom, "custom")] {
        assert_eq!(serde_json::to_value(k).unwrap(), json!(s));
    }
}

#[test]
fn j_config_deserializes_front_payload() {
    let payload = json!({
        "roots": ["/x"], "maxDepth": 4, "ignore": ["a"], "showOrphans": true,
        "terminal": {"kind":"gnomeTerminal","customExecutable":null,"customArgs":[],"claudePath":null},
        "regions": [{"id":"r","name":"R","color":"#112233","description":"d"}],
        "projects": {"/x/a": {"region":"r","tags":["t"],"dependsOn":["/x/b"],"hidden":false,"displayName":null}}
    });
    let c: Config = serde_json::from_value(payload).unwrap();
    assert_eq!(c.max_depth, 4);
    assert!(c.show_orphans);
    assert_eq!(c.terminal.kind, TerminalKind::GnomeTerminal);
    assert_eq!(c.projects["/x/a"].depends_on, ["/x/b"]);
    c.validate().unwrap();
    // snake_case não deve ser aceito silenciosamente como dado (ignorado => default): observação
    let c2: Config = serde_json::from_value(json!({"max_depth": 11})).unwrap();
    eprintln!("[OBS] snake_case max_depth ignorado silenciosamente -> maxDepth={}", c2.max_depth);
    assert_eq!(c2.max_depth, 6);
    assert!(serde_json::from_value::<Config>(json!({"terminal":{"kind":"kitty"}})).is_err());
}

#[test]
fn j_sessions_contract() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join(format!("{}.jsonl", uid(1)));
    fs::write(&p, user_line("2024-05-01T10:00:00Z", "/x", "oi")).unwrap();
    let s = sessions::parse_session_file(&p).unwrap();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        keys(&v),
        set(&["id", "title", "titleSource", "firstTimestampMs", "lastTimestampMs", "fileModifiedMs", "messageCount", "cwd", "gitBranch", "degraded", "skippedLines", "sizeBytes", "active"])
    );
    assert_eq!(v["titleSource"], "firstPrompt");
    for (t, s) in [(TitleSource::AiTitle, "aiTitle"), (TitleSource::LastPrompt, "lastPrompt"), (TitleSource::FirstPrompt, "firstPrompt"), (TitleSource::None, "none")] {
        assert_eq!(serde_json::to_value(t).unwrap(), json!(s));
    }
    let r = SessionsResult { sessions: vec![s], degraded_count: 0, unreadable: vec![UnreadableSession { file_name: "f".into(), reason: "r".into() }] };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(keys(&v), set(&["sessions", "degradedCount", "unreadable"]));
    assert_eq!(keys(&v["unreadable"][0]), set(&["fileName", "reason"]));
}

#[test]
fn j_apperror_payload() {
    let cases: Vec<(AppError, &str)> = vec![
        (AppError::Config(ConfigError::NoConfigDir), "config"),
        (AppError::Session(SessionError::Empty), "session"),
        (AppError::Launch(LaunchError::InvalidSessionId), "launch"),
        (AppError::Task("x".into()), "task"),
    ];
    for (e, k) in cases {
        let msg = e.to_string();
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(keys(&v), set(&["kind", "message"]));
        assert_eq!(v["kind"], k);
        assert_eq!(v["message"], msg);
        assert!(!msg.is_empty());
    }
}

#[test]
fn j_launch_outcome_and_terminal_status_and_loaded() {
    let v = serde_json::to_value(launcher::detect_terminals()).unwrap();
    for t in v.as_array().unwrap() {
        assert_eq!(keys(t), set(&["kind", "executable", "found"]));
    }
    let l = Config::load_from(tempfile::tempdir().unwrap().path()).unwrap();
    let v = serde_json::to_value(&l).unwrap();
    assert_eq!(keys(&v), set(&["config", "recovered", "warnings"]));
}
