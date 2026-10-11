//! Testes adversariais: categorias, bases por categoria, concessão precisa e agents_sync.
//! Só dados sintéticos em tempdirs; nunca toca em ~/.claude nem em HOME real (escrita).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use brain_lib::agents_sync::sync_dirs;
use brain_lib::bases::{BaseVia, ResolvedBase, region_of, resolve_bases_with_home};
use brain_lib::config::{Access, Config, ContextBase, ProjectMeta, Region, RegionKind, TerminalConfig, TerminalKind};
use brain_lib::launcher::{ContextArgs, LaunchMode, build_command_with};
use serde_json::Value;

// ---------------------------------------------------------------- helpers

fn canon_tmp() -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(td.path()).unwrap();
    (td, root)
}

fn mkdir(root: &Path, rel: &str) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(&p).unwrap();
    p
}

fn region(id: &str, kind: RegionKind, access: Access) -> Region {
    Region { id: id.into(), name: id.into(), kind, access, ..Region::default() }
}

fn meta(r: Option<&str>) -> ProjectMeta {
    ProjectMeta { region: r.map(String::from), ..ProjectMeta::default() }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn base(id: &str, path: &Path, targets: &[&Path], regions: &[&str]) -> ContextBase {
    ContextBase {
        id: id.into(),
        path: s(path),
        label: None,
        applies_to: targets.iter().map(|t| s(t)).collect(),
        applies_to_regions: regions.iter().map(|r| r.to_string()).collect(),
    }
}

fn config(regions: Vec<Region>, projects: &[(String, Option<&str>)], bases: Vec<ContextBase>) -> Config {
    let mut map = BTreeMap::new();
    for (k, r) in projects {
        map.insert(k.clone(), meta(*r));
    }
    Config { regions, projects: map, bases, ..Config::default() }
}

fn rb(id: &str, path: &Path, is_dir: bool, access: Access) -> ResolvedBase {
    ResolvedBase {
        id: id.into(),
        path: path.to_path_buf(),
        is_dir,
        label: None,
        inherited_from: PathBuf::from("/x"),
        access,
        via: BaseVia::Path,
    }
}

/// argv do claude (tudo depois do primeiro `--`).
fn claude_argv(ctx: &ContextArgs) -> Vec<OsString> {
    let cfg = TerminalConfig { kind: TerminalKind::Ptyxis, ..TerminalConfig::default() };
    let spec = build_command_with(&cfg, Path::new("/bin/claude"), Path::new("/tmp/proj"), &LaunchMode::New, ctx).unwrap();
    let pos = spec.args.iter().position(|a| a == "--").expect("sem --");
    // pos+1 é o binário do claude; o resto são as flags de contexto
    spec.args[pos + 2..].to_vec()
}

fn settings_of(argv: &[OsString]) -> Option<Value> {
    let idx: Vec<usize> = argv.iter().enumerate().filter(|(_, a)| *a == "--settings").map(|(i, _)| i).collect();
    assert!(idx.len() <= 1, "--settings repetido: {argv:?}");
    let i = *idx.first()?;
    let raw = argv.get(i + 1).expect("--settings sem valor").to_str().expect("json não utf8");
    Some(serde_json::from_str(raw).unwrap_or_else(|e| panic!("JSON de --settings inválido ({e}): {raw}")))
}

fn add_dirs(argv: &[OsString]) -> Vec<String> {
    argv.iter().filter_map(|a| a.to_str()?.strip_prefix("--add-dir=").map(String::from)).collect()
}

fn rules(v: &Value, key: &str) -> Vec<String> {
    v["permissions"][key]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn is_root() -> bool {
    fs::read_to_string("/proc/self/status").map(|t| t.lines().any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))).unwrap_or(false)
}

// ---------------------------------------------------------------- região efetiva

#[test]
fn regiao_efetiva_por_heranca_profunda() {
    let (_td, root) = canon_tmp();
    let topo = mkdir(&root, "w/a");
    let fundo = mkdir(&root, "w/a/b/c/d/e/f/g");
    let c = config(
        vec![region("docs", RegionKind::Source, Access::Read), region("proj", RegionKind::Consumer, Access::Read)],
        &[(s(&topo), Some("docs"))],
        vec![],
    );
    assert_eq!(region_of(&c, &fundo), Some("docs"));
    assert_eq!(region_of(&c, &topo), Some("docs"));
    assert_eq!(region_of(&c, &root.join("w")), None, "pai não herda do filho");
    assert_eq!(region_of(&c, &mkdir(&root, "w/a2")), None, "irmão com prefixo de string não herda");
    // diretório inexistente também herda (lexical)
    assert_eq!(region_of(&c, &root.join("w/a/nao/existe/ainda")), Some("docs"));
}

#[test]
fn filho_mais_especifico_sobrepoe_e_regiao_desconhecida_cai_para_ancestral() {
    let (_td, root) = canon_tmp();
    let a = mkdir(&root, "a");
    let ab = mkdir(&root, "a/b");
    let abc = mkdir(&root, "a/b/c");
    let c = config(
        vec![region("x", RegionKind::Consumer, Access::Read), region("y", RegionKind::Consumer, Access::Read)],
        &[(s(&a), Some("x")), (s(&ab), Some("y")), (s(&abc), Some("fantasma"))],
        vec![],
    );
    assert_eq!(region_of(&c, &abc), Some("y"), "região inexistente no filho: usa o ancestral válido");
    assert_eq!(region_of(&c, &ab), Some("y"));
    assert_eq!(region_of(&c, &a), Some("x"));
}

#[test]
fn regiao_com_chave_nao_normalizada() {
    let (_td, root) = canon_tmp();
    let real = mkdir(&root, "real/proj");
    let link = root.join("link");
    symlink(root.join("real"), &link).unwrap();
    let sujas = [
        format!("{}/real/./proj/", root.display()),
        format!("{}/real/x/../proj", root.display()),
        format!("{}/link/proj", root.display()), // via symlink
        format!("{}//real///proj", root.display()),
    ];
    for k in sujas {
        let c = config(vec![region("r", RegionKind::Consumer, Access::Read)], &[(k.clone(), Some("r"))], vec![]);
        assert_eq!(region_of(&c, &real), Some("r"), "chave suja: {k}");
        assert_eq!(region_of(&c, &real.join("sub/fundo")), Some("r"), "herança com chave suja: {k}");
    }
    // consulta também suja (com o trecho intermediário existente, o canonicalize resolve)
    mkdir(&root, "real/proj/x");
    let c = config(vec![region("r", RegionKind::Consumer, Access::Read)], &[(s(&real), Some("r"))], vec![]);
    assert_eq!(region_of(&c, &root.join("link/proj/./x/..")), Some("r"));
}

#[test]
fn region_of_com_chaves_hostis_nao_entra_em_panico() {
    let c = config(
        vec![region("r", RegionKind::Consumer, Access::Read)],
        &[
            ("".into(), Some("r")),
            ("relativo/sem/raiz".into(), Some("r")),
            ("/tmp/\n/\u{202e}x".into(), Some("r")),
            ("/".into(), None),
        ],
        vec![],
    );
    let _ = region_of(&c, Path::new("/"));
    let _ = region_of(&c, Path::new(""));
    let _ = region_of(&c, Path::new("/tmp/\n/\u{202e}x/y"));
    let _ = region_of(&c, Path::new("../../.."));
}

// ---------------------------------------------------------------- bases: categoria x caminho

#[test]
fn base_por_categoria_por_caminho_precedencia_e_via() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/ws/proj");
    let b_reg = mkdir(&root, "home/bases/por-categoria");
    let b_path = mkdir(&root, "home/bases/por-caminho");
    let b_both = mkdir(&root, "home/bases/ambas");
    let c = config(
        vec![region("docs", RegionKind::Consumer, Access::Read)],
        &[(s(&proj), Some("docs"))],
        vec![
            base("a-reg", &b_reg, &[], &["docs"]),
            base("b-path", &b_path, &[&proj], &[]),
            // mesma base casando pelos dois: caminho vence
            base("c-ambas", &b_both, &[&proj], &["docs"]),
            // categoria que não é a do projeto: não casa
            base("d-outra", &mkdir(&root, "home/bases/outra"), &[], &["nenhuma"]),
        ],
    );
    let r = resolve_bases_with_home(&c, &proj, Some(&home));
    let by: Vec<(&str, BaseVia)> = r.bases.iter().map(|b| (b.id.as_str(), b.via)).collect();
    assert_eq!(by.len(), 3, "{by:?} skipped={:?}", r.skipped);
    assert!(by.contains(&("a-reg", BaseVia::Region)));
    assert!(by.contains(&("b-path", BaseVia::Path)));
    assert!(by.contains(&("c-ambas", BaseVia::Path)), "caminho tem precedência sobre categoria");
    // caminho antes de categoria
    let pos = |id: &str| r.bases.iter().position(|b| b.id == id).unwrap();
    assert!(pos("a-reg") > pos("b-path") && pos("a-reg") > pos("c-ambas"));
    // ordem determinística entre os de caminho (mesmo alvo): id
    assert!(pos("b-path") < pos("c-ambas"));
    let reg = &r.bases[pos("a-reg")];
    assert_eq!(reg.inherited_from, PathBuf::from("docs"));
}

#[test]
fn mesma_base_caminho_canonico_duplicado_vence_a_de_caminho() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/p");
    let alvo = mkdir(&root, "home/b");
    let link = root.join("home/link-b");
    symlink(&alvo, &link).unwrap();
    let c = config(
        vec![region("docs", RegionKind::Consumer, Access::Read)],
        &[(s(&proj), Some("docs"))],
        vec![base("1-cat", &alvo, &[], &["docs"]), base("2-cam", &link, &[&proj], &[])],
    );
    let r = resolve_bases_with_home(&c, &proj, Some(&home));
    assert_eq!(r.bases.len(), 1, "{:?}", r.bases);
    assert_eq!(r.bases[0].id, "2-cam");
    assert_eq!(r.bases[0].via, BaseVia::Path);
}

#[test]
fn ordem_independe_da_ordem_da_config() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/p/q");
    let mut bs = Vec::new();
    for i in 0..8 {
        let b = mkdir(&root, &format!("home/b{i}"));
        if i % 2 == 0 {
            bs.push(base(&format!("id{i}"), &b, &[&proj], &[]));
        } else {
            bs.push(base(&format!("id{i}"), &b, &[], &["docs"]));
        }
    }
    let regs = vec![region("docs", RegionKind::Consumer, Access::Read)];
    let proj_map = [(s(&proj), Some("docs"))];
    let ids = |bases: Vec<ContextBase>| {
        let c = config(regs.clone(), &proj_map, bases);
        resolve_bases_with_home(&c, &proj, Some(&home)).bases.into_iter().map(|b| b.id).collect::<Vec<_>>()
    };
    let a = ids(bs.clone());
    let mut rev = bs.clone();
    rev.reverse();
    assert_eq!(a, ids(rev));
    assert_eq!(a, ids(bs.clone()), "repetível");
    assert_eq!(a.len(), 8);
}

#[test]
fn acesso_efetivo_vem_da_categoria_source_do_caminho_da_base() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/p");
    let diario = mkdir(&root, "home/diario");
    let docs = mkdir(&root, "home/docs");
    let consumer = mkdir(&root, "home/consumer");
    let c = config(
        vec![
            region("dia", RegionKind::Source, Access::Append),
            region("doc", RegionKind::Source, Access::Read),
            region("con", RegionKind::Consumer, Access::Append), // Append em consumer deve ser ignorado
            region("p", RegionKind::Consumer, Access::Read),
        ],
        &[(s(&diario), Some("dia")), (s(&docs), Some("doc")), (s(&consumer), Some("con")), (s(&proj), Some("p"))],
        vec![
            base("dia", &diario, &[&proj], &[]),
            base("doc", &docs, &[&proj], &[]),
            base("con", &consumer, &[&proj], &[]),
            base("fora", &mkdir(&root, "home/solto"), &[&proj], &[]),
        ],
    );
    let r = resolve_bases_with_home(&c, &proj, Some(&home));
    let acc = |id: &str| r.bases.iter().find(|b| b.id == id).unwrap().access;
    assert_eq!(acc("dia"), Access::Append);
    assert_eq!(acc("doc"), Access::Read);
    assert_eq!(acc("con"), Access::Read, "Append em categoria consumer não vale");
    assert_eq!(acc("fora"), Access::Read);
}

#[test]
fn base_com_alvo_ausente_vai_para_skipped_sem_panico() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/p");
    let c = config(
        vec![region("docs", RegionKind::Consumer, Access::Read)],
        &[(s(&proj), Some("docs"))],
        vec![base("sumiu", &root.join("home/nao-existe"), &[], &["docs"])],
    );
    let r = resolve_bases_with_home(&c, &proj, Some(&home));
    assert!(r.bases.is_empty());
    assert_eq!(r.skipped.len(), 1);
}

#[test]
fn resolve_com_ids_de_categoria_hostis_direto_na_struct_nao_casa_nem_panica() {
    // Config montada sem validate(): resolve não pode confiar na validação.
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "home");
    let proj = mkdir(&root, "home/p");
    let b = mkdir(&root, "home/b");
    let enorme = "x".repeat(200_000);
    let c = config(
        vec![region("docs", RegionKind::Consumer, Access::Read)],
        &[(s(&proj), Some("docs"))],
        vec![
            base("inexistente", &b, &[], &["nao-tem"]),
            base("enorme", &mkdir(&root, "home/b2"), &[], &[&enorme]),
            base("bidi", &mkdir(&root, "home/b3"), &[], &["do\u{202e}cs"]),
            base("sem-alvo", &mkdir(&root, "home/b4"), &[], &[]),
            base("dup", &mkdir(&root, "home/b5"), &[], &["docs", "docs"]),
        ],
    );
    let r = resolve_bases_with_home(&c, &proj, Some(&home));
    let ids: Vec<_> = r.bases.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["dup"], "duplicata em appliesToRegions não pode duplicar a base");
}

// ---------------------------------------------------------------- validate / carga

fn cfg_with_base(b: ContextBase) -> Config {
    Config { bases: vec![b], ..Config::default() }
}

#[test]
fn validate_rejeita_categoria_inexistente_duplicada_enorme_bidi_e_base_sem_alvo() {
    let (_td, root) = canon_tmp();
    let p = mkdir(&root, "b");
    let mk = |regs: Vec<String>, at: Vec<String>| ContextBase {
        id: "b".into(),
        path: s(&p),
        label: None,
        applies_to: at,
        applies_to_regions: regs,
    };
    assert!(cfg_with_base(mk(vec!["documentacoes".into()], vec![])).validate().is_ok(), "controle positivo");
    assert!(cfg_with_base(mk(vec!["nao-existe".into()], vec![])).validate().is_err(), "inexistente");
    assert!(cfg_with_base(mk(vec!["documentacoes".into(), "documentacoes".into()], vec![])).validate().is_err(), "duplicado");
    assert!(cfg_with_base(mk(vec!["x".repeat(1_000_000)], vec![])).validate().is_err(), "enorme");
    assert!(cfg_with_base(mk(vec!["documen\u{202e}tacoes".into()], vec![])).validate().is_err(), "bidi");
    assert!(cfg_with_base(mk(vec!["documentacoes\n".into()], vec![])).validate().is_err(), "newline");
    assert!(cfg_with_base(mk(vec![], vec![])).validate().is_err(), "sem alvos");
    // mais de 16 categorias
    let muitas: Vec<String> = (0..17).map(|i| format!("r{i}")).collect();
    let mut c = cfg_with_base(mk(muitas.clone(), vec![]));
    c.regions = muitas.iter().map(|i| region(i, RegionKind::Consumer, Access::Read)).collect();
    assert!(c.validate().is_err(), ">16 categorias");
}

#[test]
fn carga_descarta_categoria_invalida_e_base_sem_alvo_com_warning() {
    let (_td, root) = canon_tmp();
    let cfgdir = root.join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    let b = mkdir(&root, "b");
    let json = serde_json::json!({
        "bases": [
            {"id":"ok","path":s(&b),"appliesToRegions":["documentacoes"]},
            {"id":"misto","path":s(&b),"appliesToRegions":["nao-existe","documentacoes","documentacoes","do\u{202e}c", "x".repeat(300_000)]},
            {"id":"vazia","path":s(&b),"appliesToRegions":["nao-existe"]},
            {"id":"nada","path":s(&b)},
        ]
    });
    fs::write(cfgdir.join("config.json"), json.to_string()).unwrap();
    let l = Config::load_from(&cfgdir).unwrap();
    assert!(!l.recovered);
    let ids: Vec<_> = l.config.bases.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["ok", "misto"], "{:?}", l.warnings);
    assert_eq!(l.config.bases[1].applies_to_regions, vec!["documentacoes".to_string()]);
    assert!(l.warnings.len() >= 4);
    assert!(l.warnings.iter().all(|w| w.len() < 2_000), "warning não pode ecoar o id enorme inteiro");
    // a configuração carregada tem de passar na validação
    l.config.validate().unwrap();
}

#[test]
fn json_legado_sem_kind_access_applies_to_regions_carrega_com_padroes() {
    let (_td, root) = canon_tmp();
    let cfgdir = root.join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    let b = mkdir(&root, "b");
    let alvo = mkdir(&root, "alvo");
    let json = serde_json::json!({
        "regions":[{"id":"velha","name":"Velha","color":"#112233","description":"d"}],
        "projects":{ s(&alvo): {"region":"velha"} },
        "bases":[{"id":"b","path":s(&b),"appliesTo":[s(&alvo)]}]
    });
    fs::write(cfgdir.join("config.json"), json.to_string()).unwrap();
    let l = Config::load_from(&cfgdir).unwrap();
    assert!(!l.recovered, "{:?}", l.warnings);
    let r = &l.config.regions[0];
    assert_eq!((r.kind, r.access), (RegionKind::Consumer, Access::Read));
    assert!(l.config.bases[0].applies_to_regions.is_empty());
    l.config.validate().unwrap();
    // round-trip: salvar e recarregar mantém tudo
    l.config.save_to(&cfgdir).unwrap();
    let l2 = Config::load_from(&cfgdir).unwrap();
    assert_eq!(l2.config.regions, l.config.regions);
    assert_eq!(l2.config.bases, l.config.bases);
}

#[test]
fn regiao_invalida_de_valor_de_enum_cai_em_recuperacao_sem_perder_o_original() {
    let (_td, root) = canon_tmp();
    let cfgdir = root.join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    let json = r##"{"regions":[{"id":"a","name":"A","color":"#112233","kind":"frontal"}]}"##;
    fs::write(cfgdir.join("config.json"), json).unwrap();
    let l = Config::load_from(&cfgdir).unwrap();
    assert!(l.recovered, "kind desconhecido não é parseável: deve recuperar");
    let baks: Vec<_> = fs::read_dir(&cfgdir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().contains(".bak")).collect();
    assert_eq!(baks.len(), 1, "o original precisa ficar guardado");
    assert_eq!(fs::read_to_string(baks[0].path()).unwrap(), json);
}

#[test]
fn projeto_com_regiao_antiga_frontal_na_carga() {
    let (_td, root) = canon_tmp();
    let cfgdir = root.join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    let proj = mkdir(&root, "p");
    let json = serde_json::json!({ "projects": { s(&proj): {"region":"frontal"}, "relativo/x": {"region":"projetos"} } });
    fs::write(cfgdir.join("config.json"), json.to_string()).unwrap();
    let l = Config::load_from(&cfgdir).unwrap();
    assert!(!l.recovered);
    // Região fora do conjunto não pode sobreviver: senão save_to/validate falha para sempre.
    let ainda = l.config.projects.get(&s(&proj)).and_then(|m| m.region.clone());
    assert_ne!(ainda.as_deref(), Some("frontal"), "região órfã 'frontal' sobreviveu à carga; warnings={:?}", l.warnings);
}

#[test]
fn config_carregada_com_regiao_antiga_precisa_ser_regravavel() {
    let (_td, root) = canon_tmp();
    let cfgdir = root.join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    let proj = mkdir(&root, "p");
    let json = serde_json::json!({ "projects": { s(&proj): {"region":"frontal"} } });
    fs::write(cfgdir.join("config.json"), json.to_string()).unwrap();
    let l = Config::load_from(&cfgdir).unwrap();
    l.config.save_to(&cfgdir).expect("config carregada deve ser regravável (UI não consegue salvar nada se falhar)");
}

// ---------------------------------------------------------------- launcher: concessão precisa

#[test]
fn base_pasta_vira_add_dir_e_deny_edit_ou_ask() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let ler = mkdir(&root, "h/ler");
    let esc = mkdir(&root, "h/escrever");
    let bases = vec![rb("ler", &ler, true, Access::Read), rb("esc", &esc, true, Access::Append)];
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&bases, Some(&home));
    assert!(skipped.is_empty(), "{skipped:?}");
    let argv = claude_argv(&ctx);
    assert_eq!(add_dirs(&argv), vec![s(&ler), s(&esc)]);
    let v = settings_of(&argv).unwrap();
    assert_eq!(rules(&v, "deny"), vec![format!("Edit(/{}/**)", ler.display())]);
    assert_eq!(rules(&v, "ask"), vec![format!("Edit(/{}/**)", esc.display())]);
    assert!(rules(&v, "allow").is_empty());
    assert_eq!(argv.iter().filter(|a| *a == "--append-system-prompt").count(), 1);
}

#[test]
fn base_arquivo_nunca_gera_add_dir_nem_concede_a_pasta_pai() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let pai = mkdir(&root, "h/pai");
    let f = pai.join("nota.md");
    fs::write(&f, "x").unwrap();
    let g = pai.join("outra.md");
    fs::write(&g, "x").unwrap();
    let bases = vec![rb("f", &f, false, Access::Read), rb("g", &g, false, Access::Append)];
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&bases, Some(&home));
    assert!(skipped.is_empty());
    let argv = claude_argv(&ctx);
    assert!(add_dirs(&argv).is_empty(), "arquivo gerou --add-dir: {argv:?}");
    assert!(!argv.iter().any(|a| a.to_string_lossy().contains("--add-dir")));
    let v = settings_of(&argv).unwrap();
    let allow = rules(&v, "allow");
    assert_eq!(allow, vec![format!("Read(/{})", f.display()), format!("Read(/{})", g.display())]);
    assert!(allow.iter().all(|r| !r.contains("/**")), "nenhuma regra de pasta: {allow:?}");
    assert_eq!(rules(&v, "deny"), vec![format!("Edit(/{})", f.display())]);
    assert_eq!(rules(&v, "ask"), vec![format!("Edit(/{})", g.display())]);
    assert!(!v.to_string().contains(&format!("{}/**", pai.display())));
}

#[test]
fn prompt_only_nao_gera_regras_nem_add_dir_mas_aparece_no_prompt() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&[rb("amplo", &home, true, Access::Read)], Some(&home));
    assert!(skipped.is_empty());
    assert_eq!(ctx.applied(), 0, "PromptOnly não conta como aplicada (só no prompt)");
    let argv = claude_argv(&ctx);
    assert!(settings_of(&argv).is_none(), "PromptOnly gerou --settings: {argv:?}");
    assert!(add_dirs(&argv).is_empty());
    assert!(ctx.prompt().unwrap().contains(&s(&home)));
    // misto: o amplo não contamina o específico
    let esp = mkdir(&root, "h/esp");
    let (ctx, _) = ContextArgs::from_bases_with_home(&[rb("amplo", &home, true, Access::Read), rb("esp", &esp, true, Access::Read)], Some(&home));
    let argv = claude_argv(&ctx);
    assert_eq!(add_dirs(&argv), vec![s(&esp)]);
    let v = settings_of(&argv).unwrap();
    assert_eq!(rules(&v, "deny").len(), 1);
}

#[test]
fn caminhos_proibidos_nao_geram_regra_nem_entram_no_prompt() {
    let home = Path::new("/home/u");
    let bases = vec![
        rb("ssh", Path::new("/home/u/.ssh"), true, Access::Read),
        rb("claude", Path::new("/home/u/.claude/agents/x.md"), false, Access::Read),
        rb("etc", Path::new("/etc/shadow"), false, Access::Read),
        rb("raiz", Path::new("/"), true, Access::Read),
    ];
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&bases, Some(home));
    assert_eq!(skipped.len(), 4, "{skipped:?}");
    assert_eq!(ctx.applied(), 0);
    let argv = claude_argv(&ctx);
    assert!(argv.is_empty(), "{argv:?}");
}

#[test]
fn caracteres_especiais_nunca_viram_regra_e_vao_para_skipped() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let nomes = [
        "a*b", "a?b", "a[b", "a]b", "a(b", "a)b", "a{b", "a}b", "a!b", "a#b", "a\\b", "a\"b", "a'b", "a`b", "a,b",
        "a\nb", "a\u{202e}b", "a\u{200b}b", "a\tb",
    ];
    for n in nomes {
        let d = home.join(n);
        fs::create_dir_all(&d).unwrap();
        let f = home.join(format!("f-{n}"));
        fs::write(&f, "x").unwrap();
        // 1) via from_bases direto
        for (path, is_dir) in [(&d, true), (&f, false)] {
            let (ctx, skipped) = ContextArgs::from_bases_with_home(&[rb("x", path, is_dir, Access::Read)], Some(&home));
            assert_eq!(skipped.len(), 1, "{n:?} deveria ir para skipped");
            assert_eq!(ctx.applied(), 0, "{n:?} não pode entrar no prompt");
            assert!(claude_argv(&ctx).is_empty(), "{n:?} gerou argv");
        }
        // 2) via resolve_bases (config -> resolve)
        let proj = mkdir(&root, "h/proj");
        let c = config(vec![], &[], vec![base("x", &d, &[&proj], &[])]);
        let r = resolve_bases_with_home(&c, &proj, Some(&home));
        assert!(r.bases.is_empty(), "{n:?} sobreviveu ao resolve");
        assert_eq!(r.skipped.len(), 1, "{n:?}");
    }
}

#[test]
fn nomes_comuns_porem_incomodos_geram_json_valido_em_um_unico_argv() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let mut bases = Vec::new();
    for n in ["com espaço", "com$dolar", "açaí-ção", "$(id)", "~tilde", "-menos", "--add-dir=x", "a:b", "a;b", "a&b", "a|b", "a<b"] {
        let d = mkdir(&root, &format!("h/{n}"));
        bases.push(rb(n, &d, true, Access::Read));
        let f = home.join(format!("{n}.txt"));
        fs::write(&f, "x").unwrap();
        bases.push(rb(&format!("{n}-f"), &f, false, Access::Append));
    }
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&bases, Some(&home));
    assert!(skipped.len() <= 4, "{skipped:?}"); // limite de 20 bases: 24 candidatas
    let argv = claude_argv(&ctx);
    let v = settings_of(&argv).expect("deve haver --settings");
    assert!(v["permissions"].is_object());
    // todo --add-dir é UM elemento de argv com "=" e nenhum argumento solto vira flag
    for a in &argv {
        let t = a.to_string_lossy();
        if t.starts_with("--") {
            assert!(["--settings", "--append-system-prompt"].contains(&t.as_ref()) || t.starts_with("--add-dir="), "{t}");
        }
    }
    // cada elemento que veio de um caminho com espaço continua inteiro
    assert!(add_dirs(&argv).iter().any(|d| d.ends_with("com espaço")));
    // o número de elementos: N add-dirs + 2 (settings) + 2 (prompt)
    assert_eq!(argv.len(), add_dirs(&argv).len() + 4, "{argv:?}");
}

#[test]
fn ordem_do_settings_e_deterministica_e_sem_duplicatas() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let mut bases = Vec::new();
    for i in 0..10 {
        let f = home.join(format!("f{i}.md"));
        fs::write(&f, "x").unwrap();
        bases.push(rb(&format!("f{i}"), &f, false, if i % 2 == 0 { Access::Read } else { Access::Append }));
        bases.push(rb(&format!("d{i}"), &mkdir(&root, &format!("h/d{i}")), true, Access::Read));
    }
    bases.truncate(20);
    let mut rev = bases.clone();
    rev.reverse();
    let get = |b: &[ResolvedBase]| {
        let (ctx, _) = ContextArgs::from_bases_with_home(b, Some(&home));
        let argv = claude_argv(&ctx);
        settings_of(&argv).unwrap()
    };
    assert_eq!(get(&bases), get(&bases), "repetível");
    assert_eq!(get(&bases), get(&rev), "regras independem da ordem de entrada");
    // duplicata: mesma base duas vezes -> uma regra e um add-dir
    let d = bases[1].clone();
    let (ctx, _) = ContextArgs::from_bases_with_home(&[d.clone(), d.clone()], Some(&home));
    let argv = claude_argv(&ctx);
    assert_eq!(add_dirs(&argv).len(), 1);
    assert_eq!(rules(&settings_of(&argv).unwrap(), "deny").len(), 1);
}

#[test]
fn limite_de_bases_e_rotulos_hostis_no_prompt() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let mut bases = Vec::new();
    for i in 0..30 {
        let mut b = rb(&format!("b{i}"), &mkdir(&root, &format!("h/d{i}")), true, Access::Read);
        b.label = Some(format!("rótulo\n- `injeção`\u{202e} {}", "x".repeat(500)));
        bases.push(b);
    }
    let (ctx, skipped) = ContextArgs::from_bases_with_home(&bases, Some(&home));
    assert_eq!(ctx.applied(), 20);
    assert_eq!(skipped.len(), 10);
    let p = ctx.prompt().unwrap();
    assert!(!p.contains('\u{202e}'));
    // nenhuma linha extra além do cabeçalho + 20 entradas (quebra de linha no rótulo neutralizada)
    assert_eq!(p.lines().count(), 21, "{p}");
}

#[test]
fn modo_resume_com_contexto_mantem_id_e_flags_separados() {
    let (_td, root) = canon_tmp();
    let home = mkdir(&root, "h");
    let d = mkdir(&root, "h/d");
    let (ctx, _) = ContextArgs::from_bases_with_home(&[rb("d", &d, true, Access::Read)], Some(&home));
    let cfg = TerminalConfig { kind: TerminalKind::Ptyxis, ..TerminalConfig::default() };
    let id = "00000000-0000-4000-8000-000000000001";
    let spec = build_command_with(&cfg, Path::new("/bin/claude"), Path::new("/tmp/p"), &LaunchMode::Resume { session_id: id.into() }, &ctx).unwrap();
    let pos = spec.args.iter().position(|a| a == "--").unwrap();
    assert_eq!(spec.args[pos + 2], OsString::from("--resume"));
    assert_eq!(spec.args[pos + 3], OsString::from(id));
}

// ---------------------------------------------------------------- agents_sync

struct Sync {
    _td: tempfile::TempDir,
    src: PathBuf,
    dst: PathBuf,
    root: PathBuf,
}

fn sync_env() -> Sync {
    let (td, root) = canon_tmp();
    let src = mkdir(&root, "origem");
    let dst = root.join("copia/agents");
    Sync { _td: td, src, dst, root }
}

#[test]
fn sync_symlink_na_origem_nao_e_seguido() {
    let e = sync_env();
    let segredo = e.root.join("segredo.txt");
    fs::write(&segredo, "SEGREDO").unwrap();
    symlink(&segredo, e.src.join("vaza.md")).unwrap();
    symlink("/etc", e.src.join("dir.md")).unwrap();
    symlink(e.root.join("nao-existe"), e.src.join("quebrado.md")).unwrap();
    fs::write(e.src.join("ok.md"), "ok").unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 1, "{r:?}");
    for n in ["vaza.md", "dir.md", "quebrado.md"] {
        assert!(fs::symlink_metadata(e.dst.join(n)).is_err(), "{n} foi copiado");
    }
}

#[test]
fn sync_symlink_no_destino_nao_escreve_atraves_dele() {
    let e = sync_env();
    fs::write(e.src.join("a.md"), "novo").unwrap();
    fs::write(e.src.join("b.md"), "novo").unwrap();
    fs::create_dir_all(&e.dst).unwrap();
    let alvo = e.root.join("alvo-externo.txt");
    fs::write(&alvo, "INTACTO").unwrap();
    symlink(&alvo, e.dst.join("a.md")).unwrap();
    symlink(e.root.join("nao-existe"), e.dst.join("b.md")).unwrap(); // pendente
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 0, "{r:?}");
    assert_eq!(r.skipped.len(), 2);
    assert_eq!(fs::read_to_string(&alvo).unwrap(), "INTACTO");
    assert!(!e.root.join("nao-existe").exists(), "criou o alvo do symlink pendente");
}

#[test]
fn sync_destino_inteiro_e_symlink_para_outro_diretorio() {
    // Destino que é symlink é recusado: nada é escrito no alvo.
    let e = sync_env();
    fs::write(e.src.join("a.md"), "a").unwrap();
    let real = mkdir(&e.root, "outro-lugar");
    fs::create_dir_all(e.dst.parent().unwrap()).unwrap();
    symlink(&real, &e.dst).unwrap();
    assert!(sync_dirs(&e.src, &e.dst).is_err());
    assert!(!real.join("a.md").exists());
}

#[test]
fn sync_arquivo_acima_de_256kib_e_limite_exato() {
    let e = sync_env();
    fs::write(e.src.join("exato.md"), vec![b'x'; 256 * 1024]).unwrap();
    fs::write(e.src.join("mais1.md"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    fs::write(e.src.join("gigante.md"), vec![b'x'; 5 * 1024 * 1024]).unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 1);
    assert_eq!(r.skipped.len(), 2, "{r:?}");
    assert!(!e.dst.join("mais1.md").exists() && !e.dst.join("gigante.md").exists());
}

#[test]
fn sync_mais_de_200_arquivos_e_limite_exato() {
    let e = sync_env();
    for i in 0..200 {
        fs::write(e.src.join(format!("a{i:03}.md")), "x").unwrap();
    }
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!((r.copied, r.skipped.len()), (200, 0), "200 exatos");
    fs::write(e.src.join("z999.md"), "x").unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!((r.copied, r.unchanged, r.skipped.len()), (0, 200, 1), "{r:?}");
    assert!(r.stale.is_empty(), "o arquivo acima do limite existe na origem; não é stale");
    // inválidos não consomem o limite
    let e = sync_env();
    for i in 0..10 {
        fs::write(e.src.join(format!("ruim\u{202e}{i}.md")), "x").unwrap();
    }
    for i in 0..200 {
        fs::write(e.src.join(format!("b{i:03}.md")), "x").unwrap();
    }
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 200, "{:?}", r.skipped);
}

#[test]
fn sync_nomes_com_controle_bidi_e_nao_utf8() {
    let e = sync_env();
    for n in ["a\nb.md", "a\tb.md", "a\u{202e}b.md", "a\u{200b}b.md", "a\\b.md", ".md", "ok ção.md"] {
        fs::write(e.src.join(n), "x").unwrap();
    }
    fs::write(e.src.join(OsStr::from_bytes(b"inv\xff\xfe.md")), "x").unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 1, "{r:?}");
    assert!(e.dst.join("ok ção.md").exists());
    assert_eq!(r.skipped.len(), 6, "{:?}", r.skipped);
    for sk in &r.skipped {
        assert!(!sk.contains('\n') && !sk.contains('\u{202e}'), "skipped ecoa caractere perigoso: {sk:?}");
    }
    let n: Vec<_> = fs::read_dir(&e.dst).unwrap().collect();
    assert_eq!(n.len(), 1, "só o nome válido pode existir no destino");
}

#[test]
fn sync_origem_ausente_ou_arquivo() {
    let e = sync_env();
    let r = sync_dirs(&e.root.join("nada"), &e.dst).unwrap();
    assert!(r.source_missing && !e.dst.exists());
    // origem é um arquivo (não diretório): não deve criar destino nem panicar
    let f = e.root.join("arq");
    fs::write(&f, "x").unwrap();
    let res = sync_dirs(&f, &e.dst);
    eprintln!("OBSERVACAO origem-arquivo: {res:?}");
    assert!(!e.dst.join("x").exists());
}

#[test]
fn sync_idempotente_e_preserva_mtime() {
    let e = sync_env();
    for i in 0..5 {
        fs::write(e.src.join(format!("a{i}.md")), format!("conteudo {i}")).unwrap();
    }
    let r1 = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r1.copied, 5);
    let snap = |d: &Path| -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|x| {
                let x = x.unwrap();
                (x.file_name().to_string_lossy().into_owned(), fs::read(x.path()).unwrap(), x.metadata().unwrap().modified().unwrap())
            })
            .collect();
        v.sort();
        v
    };
    let antes = snap(&e.dst);
    let r2 = sync_dirs(&e.src, &e.dst).unwrap();
    let r3 = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!((r2.copied, r2.unchanged), (0, 5));
    assert_eq!(r2, r3);
    assert_eq!(antes, snap(&e.dst));
    // nenhum temporário sobrando
    assert!(fs::read_dir(&e.dst).unwrap().all(|x| !x.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    // permissões
    assert_eq!(fs::metadata(e.dst.join("a0.md")).unwrap().permissions().mode() & 0o777, 0o644);
}

#[test]
fn sync_destino_somente_leitura() {
    if is_root() {
        eprintln!("root ignora permissões: teste não aplicável");
        return;
    }
    let e = sync_env();
    fs::write(e.src.join("a.md"), "a").unwrap();
    fs::create_dir_all(&e.dst).unwrap();
    fs::set_permissions(&e.dst, fs::Permissions::from_mode(0o555)).unwrap();
    let res = sync_dirs(&e.src, &e.dst);
    fs::set_permissions(&e.dst, fs::Permissions::from_mode(0o755)).unwrap(); // para o tempdir limpar
    let r = res.expect("destino RO deve virar skipped, não erro fatal nem panic");
    assert_eq!(r.copied, 0);
    assert_eq!(r.skipped.len(), 1, "{r:?}");
    assert!(!e.dst.join("a.md").exists());
    assert_eq!(fs::read_to_string(e.src.join("a.md")).unwrap(), "a");
}

#[test]
fn sync_destino_nao_criavel_devolve_erro_sem_panico() {
    if is_root() {
        return;
    }
    let e = sync_env();
    fs::write(e.src.join("a.md"), "a").unwrap();
    let pai = mkdir(&e.root, "ro");
    fs::set_permissions(&pai, fs::Permissions::from_mode(0o555)).unwrap();
    let res = sync_dirs(&e.src, &pai.join("sub/agents"));
    fs::set_permissions(&pai, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(res.is_err());
}

#[test]
fn sync_arquivo_destino_somente_leitura_e_substituido_sem_apagar_outros() {
    let e = sync_env();
    fs::write(e.src.join("a.md"), "versao 2 maior").unwrap();
    fs::create_dir_all(&e.dst).unwrap();
    fs::write(e.dst.join("a.md"), "v1").unwrap();
    fs::set_permissions(e.dst.join("a.md"), fs::Permissions::from_mode(0o444)).unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.copied, 1, "{r:?}");
    assert_eq!(fs::read_to_string(e.dst.join("a.md")).unwrap(), "versao 2 maior");
}

#[test]
fn sync_arquivo_removido_da_origem_vai_para_stale_e_nao_e_apagado() {
    let e = sync_env();
    fs::write(e.src.join("a.md"), "a").unwrap();
    fs::write(e.src.join("b.md"), "b").unwrap();
    sync_dirs(&e.src, &e.dst).unwrap();
    fs::remove_file(e.src.join("b.md")).unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.stale, vec!["b.md".to_string()]);
    assert_eq!(fs::read_to_string(e.dst.join("b.md")).unwrap(), "b");
    // outros arquivos no destino (não .md) nunca são tocados nem listados
    fs::write(e.dst.join("anotacao.txt"), "minha").unwrap();
    let r = sync_dirs(&e.src, &e.dst).unwrap();
    assert_eq!(r.stale, vec!["b.md".to_string()]);
    assert_eq!(fs::read_to_string(e.dst.join("anotacao.txt")).unwrap(), "minha");
    // origem vazia: nada é apagado do destino
    let vazio = mkdir(&e.root, "vazio");
    let r = sync_dirs(&vazio, &e.dst).unwrap();
    assert_eq!(r.stale.len(), 2);
    assert!(e.dst.join("a.md").exists() && e.dst.join("b.md").exists());
}

#[test]
fn sync_origem_nunca_e_modificada() {
    let e = sync_env();
    fs::write(e.src.join("a.md"), "a").unwrap();
    fs::set_permissions(&e.src, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(e.src.join("a.md"), fs::Permissions::from_mode(0o444)).unwrap();
    let antes = fs::metadata(e.src.join("a.md")).unwrap().modified().unwrap();
    let r = sync_dirs(&e.src, &e.dst);
    fs::set_permissions(&e.src, fs::Permissions::from_mode(0o755)).unwrap();
    let r = r.unwrap();
    assert_eq!(r.copied, 1);
    assert_eq!(fs::metadata(e.src.join("a.md")).unwrap().modified().unwrap(), antes);
    assert_eq!(fs::read_to_string(e.src.join("a.md")).unwrap(), "a");
}
