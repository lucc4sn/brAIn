//! Utilitários de sistema de arquivos compartilhados entre módulos.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Grava de forma atômica: arquivo temporário de nome ÚNICO (pid + contador +
/// timestamp, aberto com `create_new`) seguido de `rename`. Escritas
/// concorrentes nunca compartilham o temporário.
pub fn write_atomic(target: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("destino sem nome de arquivo"))?
        .to_string_lossy();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{}.{n}.{nanos}.tmp", std::process::id()));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        // O temporário é nosso (nome único); limpar é seguro.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Caractere inseguro em caminhos, rótulos e alvos: controle (inclui NUL e
/// quebras de linha) e invisíveis/bidi que enganam a leitura (U+200B..200F,
/// U+2028..202E, U+2060..2064, U+2066..2069, U+FEFF).
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Máximo de caracteres de texto do usuário ecoados em avisos/motivos.
pub const MAX_ECHO_CHARS: usize = 80;

/// Para ecoar texto do usuário (id, rótulo, caminho) em avisos: trunca em
/// `MAX_ECHO_CHARS` caracteres e aplica `escape_debug` ao truncado.
pub fn clip_debug(s: &str) -> String {
    s.chars().take(MAX_ECHO_CHARS).collect::<String>().escape_debug().to_string()
}

/// Arquivo regular com algum bit de execução.
pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Expande `~` / `~/...` para o home; qualquer outra coisa passa intacta.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        if s == "~" {
            return home;
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grava_e_nao_deixa_temporarios() {
        let d = tempfile::tempdir().unwrap();
        let t = d.path().join("a.json");
        write_atomic(&t, b"1", 0o600).unwrap();
        write_atomic(&t, b"22", 0o600).unwrap();
        assert_eq!(fs::read(&t).unwrap(), b"22");
        assert_eq!(fs::metadata(&t).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn caracteres_inseguros() {
        for c in ['\0', '\n', '\u{7}', '\u{200B}', '\u{202E}', '\u{2028}', '\u{2066}', '\u{FEFF}', '\u{2060}'] {
            assert!(is_unsafe_char(c), "{c:?}");
        }
        for c in ['a', ' ', 'ç', '日', '😀', '\u{2010}'] {
            assert!(!is_unsafe_char(c), "{c:?}");
        }
    }

    #[test]
    fn clip_trunca_e_escapa() {
        assert_eq!(clip_debug("a\nb"), "a\\nb");
        assert_eq!(clip_debug(&"x".repeat(500)).len(), MAX_ECHO_CHARS);
    }

    #[test]
    fn tilde() {
        assert_eq!(expand_tilde("/x"), PathBuf::from("/x"));
        assert_eq!(expand_tilde("rel"), PathBuf::from("rel"));
        if let Some(h) = dirs::home_dir() {
            assert_eq!(expand_tilde("~/a"), h.join("a"));
        }
    }
}
