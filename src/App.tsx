import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as pickPath } from "@tauri-apps/plugin-dialog";
import { Settings } from "./Settings";
import { Palette, type PaletteAction } from "./Palette";
import { FolderGraph, type ConsultLink, type FocusRequest } from "./graph/FolderGraph";
import {
  api, errorMessage, relativeTime,
  type Config, type ContextBase, type Region, type LaunchMode, type ProjectMeta, type ResolvedBases, type SessionsResult, type TreeNode, type TreeResult,
} from "./lib/api";
import { ancestors, baseName, buildIndex, colorFor, deepestNodeFor, effectiveRegions, isUnder, visibleNodes, type TreeIndex } from "./lib/tree";
import "./styles/tokens.css";
import "./App.css";

const SIDEBAR_W = 260;
const DETAIL_W = 340;
const NEUTRAL = "#6b7080";
const UI_KEY = "brain.ui";
interface SavedUi { expanded: string[]; selected: string | null }
function loadUi(): SavedUi | null {
  try {
    const v = JSON.parse(localStorage.getItem(UI_KEY) ?? "null") as SavedUi | null;
    if (v && Array.isArray(v.expanded)) return { expanded: v.expanded.filter((x) => typeof x === "string"), selected: typeof v.selected === "string" ? v.selected : null };
  } catch { /* storage indisponível ou corrompido: começa limpo */ }
  return null;
}
/** Tira o prefixo técnico "base <uuid>: " dos motivos devolvidos pelo backend. */
const cleanReason = (r: string) => r.replace(/^base [0-9a-fA-F-]{8,}: /, "");
const EMPTY_IDX: TreeIndex = { byId: new Map(), children: new Map(), roots: [], topLevels: [] };

// Scaffold neutro: a UI final virá do Claude Design (ver design-prompts/).
export default function App() {
  const [config, setConfig] = useState<Config | null>(null);
  const [tree, setTree] = useState<TreeResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [showPalette, setShowPalette] = useState(false);
  const [agentsDir, setAgentsDir] = useState<string | null>(null);

  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [sessions, setSessions] = useState<SessionsResult | null>(null);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [hiddenGroups, setHiddenGroups] = useState<Set<string>>(new Set());
  const [colorMode, setColorMode] = useState<"region" | "folder">(() => {
    try { return localStorage.getItem("brain.colorMode") === "folder" ? "folder" : "region"; } catch { return "region"; }
  });
  const [resolved, setResolved] = useState<ResolvedBases | null>(null);
  const [showLabels, setShowLabels] = useState(true);
  const [focusMode, setFocusMode] = useState(true);
  const [query, setQuery] = useState("");
  const [focusReq, setFocusReq] = useState<FocusRequest | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const nonce = useRef(0);

  const idx = useMemo(() => (tree ? buildIndex(tree.nodes, tree.rootIds) : EMPTY_IDX), [tree]);
  const palette = useMemo(() => config?.regions.map((r) => r.color) ?? [], [config]);

  const regions = config?.regions ?? [];
  const regionById = useMemo(() => new Map(regions.map((r) => [r.id, r])), [regions]);
  const ownRegion = useCallback((n: TreeNode) => {
    const id = config?.projects[n.path]?.region ?? null;
    return id && regionById.has(id) ? id : null;
  }, [config, regionById]);
  const regionOf = useMemo(() => effectiveRegions(tree?.nodes ?? [], ownRegion), [tree, ownRegion]);
  const colorOf = useCallback((n: TreeNode) => {
    if (colorMode === "region") {
      const r = regionOf.get(n.id);
      return n.depth === 0 && !r ? "#c9ccd6" : r ? regionById.get(r)?.color ?? NEUTRAL : NEUTRAL;
    }
    return colorFor(idx, palette, n);
  }, [colorMode, regionOf, regionById, idx, palette]);
  const groupKeyOf = useCallback((n: TreeNode) => (colorMode === "region" ? regionOf.get(n.id) ?? "" : n.topLevel), [colorMode, regionOf]);
  const isHidden = useCallback((n: TreeNode) => {
    const k = groupKeyOf(n);
    return k !== "" && hiddenGroups.has(k);
  }, [groupKeyOf, hiddenGroups]);

  const requestFocus = useCallback((ids: string[]) => setFocusReq({ ids, nonce: ++nonce.current }), []);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const { config: cfg, recovered, warnings } = await api.getConfig();
      setConfig(cfg);
      if (recovered) setToast("A configuração estava inválida e foi recuperada; uma cópia ficou em config.json.bak.");
      else if (warnings?.length) setToast(`Configuração ajustada ao carregar (${warnings.length} aviso(s)): ${warnings[0]}`);
      const t = await api.scanTree();
      const i = buildIndex(t.nodes, t.rootIds);
      setTree(t);
      const open = new Set(i.roots); // raiz aberta: mostra o primeiro nível
      const saved = loadUi();
      saved?.expanded.forEach((id) => { if (i.byId.has(id)) open.add(id); });
      setExpanded(open);
      if (saved?.selected && i.byId.has(saved.selected)) setSelected(saved.selected);
      requestFocus(visibleNodes(i, open).map((n) => n.id));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [requestFocus]);

  useEffect(() => { void load(); }, [load]);
  useEffect(() => { api.agentsCopyDir().then(setAgentsDir).catch(() => setAgentsDir(null)); }, []);

  // Persiste pastas abertas e seleção (só depois da primeira carga, para não apagar o salvo).
  useEffect(() => {
    if (!tree) return;
    const t = setTimeout(() => {
      try { localStorage.setItem(UI_KEY, JSON.stringify({ expanded: [...expanded], selected } satisfies SavedUi)); } catch { /* ok */ }
    }, 300);
    return () => clearTimeout(t);
  }, [tree, expanded, selected]);

  // Busca sobre TODA a árvore; ancestrais dos achados ficam visíveis.
  const matching = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q || !tree) return null;
    return new Set(tree.nodes.filter((n) => n.name.toLowerCase().includes(q) || n.path.toLowerCase().includes(q)).map((n) => n.id));
  }, [query, tree]);

  const effectiveExpanded = useMemo(() => {
    if (!matching) return expanded;
    const s = new Set(expanded);
    matching.forEach((id) => ancestors(idx, id).forEach((a) => s.add(a)));
    return s;
  }, [expanded, matching, idx]);

  const visible = useMemo(() => visibleNodes(idx, effectiveExpanded, isHidden), [idx, effectiveExpanded, isHidden]);
  const node = selected ? idx.byId.get(selected) ?? null : null;

  const toggle = useCallback((id: string) => {
    const n = idx.byId.get(id);
    if (!n || n.childCount === 0) return;
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
    const opening = !expanded.has(id);
    requestFocus(opening ? [id, ...(idx.children.get(id) ?? [])] : [id, ...(n.parent ? [n.parent] : [])]);
  }, [idx, expanded, requestFocus]);

  const select = useCallback((id: string | null, camera = false) => {
    setSelected(id);
    if (id && camera) requestFocus([id, ...(idx.byId.get(id)?.parent ? [idx.byId.get(id)?.parent as string] : [])]);
  }, [idx, requestFocus]);

  // Painel: sessões da pasta selecionada.
  useEffect(() => {
    setSessions(null);
    setSessionId(null);
    if (!node) return;
    let cancelled = false;
    api.listSessions(node.path)
      .then((r) => { if (!cancelled) setSessions(r); })
      .catch((e) => { if (!cancelled) setError(errorMessage(e)); });
    return () => { cancelled = true; };
  }, [node?.path]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 5000);
    return () => clearTimeout(t);
  }, [toast]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && e.key === ",") { e.preventDefault(); setShowSettings(true); return; }
      if (e.ctrlKey && e.key.toLowerCase() === "k") { e.preventDefault(); setShowPalette(true); return; }
      if (e.key === "Escape") { if (query) setQuery(""); else setSelected(null); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [query]);

  const onTreeKey = (e: React.KeyboardEvent) => {
    if (!["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Enter"].includes(e.key)) return;
    e.preventDefault();
    const i = visible.findIndex((n) => n.id === selected);
    const cur = i >= 0 ? visible[i] : null;
    if (e.key === "ArrowDown") select(visible[Math.min(visible.length - 1, i + 1)]?.id ?? null);
    else if (e.key === "ArrowUp") select(visible[Math.max(0, i - 1)]?.id ?? null);
    else if (!cur) return;
    else if (e.key === "Enter") toggle(cur.id);
    else if (e.key === "ArrowRight") {
      if (cur.childCount > 0 && !effectiveExpanded.has(cur.id)) toggle(cur.id);
      else select(idx.children.get(cur.id)?.[0] ?? cur.id);
    } else if (e.key === "ArrowLeft") {
      if (effectiveExpanded.has(cur.id) && cur.childCount > 0) toggle(cur.id);
      else if (cur.parent) select(cur.parent);
    }
  };

  const launch = async (mode: LaunchMode) => {
    if (!node) return;
    try {
      const out = await api.launch(node.path, mode);
      setToast(`Terminal aberto em ${node.name} (${out.terminalUsed})${out.basesApplied ? ` · ${out.basesApplied} base(s) de contexto` : ""}${out.basesSkipped.length ? ` · ${out.basesSkipped.length} ignorada(s)` : ""}`);
    } catch (e) {
      setToast(`Não foi possível abrir o terminal: ${errorMessage(e)}`);
    }
  };

  const groups = useMemo(() => {
    const nodes = tree?.nodes ?? [];
    if (colorMode === "region") {
      const count = new Map<string, number>();
      nodes.forEach((n) => { if (n.isProject) { const r = regionOf.get(n.id); if (r) count.set(r, (count.get(r) ?? 0) + 1); } });
      return regions.map((r) => ({ key: r.id, name: r.name, color: r.color, count: count.get(r.id) ?? 0, desc: r.description }));
    }
    return idx.topLevels.map((name) => {
      const top = nodes.find((n) => n.depth === 1 && n.name === name);
      return { key: name, name, color: top ? colorFor(idx, palette, top) : NEUTRAL, count: top ? top.descendantCount + 1 : 0, desc: "" };
    });
  }, [tree, colorMode, regions, regionOf, idx, palette]);
  const toggleGroup = (k: string) =>
    setHiddenGroups((prev) => { const n = new Set(prev); if (n.has(k)) n.delete(k); else n.add(k); return n; });
  const changeColorMode = (m: "region" | "folder") => {
    setColorMode(m);
    setHiddenGroups(new Set());
    try { localStorage.setItem("brain.colorMode", m); } catch { /* sem storage: ok */ }
  };

  // Persistência de config (regiões e bases) com atualização otimista do estado local.
  const updateConfig = useCallback(async (mutate: (c: Config) => void) => {
    if (!config) return;
    const next: Config = structuredClone(config);
    mutate(next);
    try {
      await api.saveConfig(next);
      setConfig(next);
    } catch (e) {
      setToast(`Não foi possível salvar: ${errorMessage(e)}`);
    }
  }, [config]);

  const setRegion = (n: TreeNode, regionId: string | null) =>
    void updateConfig((c) => {
      const meta: ProjectMeta = c.projects[n.path] ?? { region: null, tags: [], dependsOn: [], hidden: false, displayName: null };
      meta.region = regionId;
      const empty = !meta.region && !meta.tags.length && !meta.dependsOn.length && !meta.hidden && !meta.displayName;
      if (empty) delete c.projects[n.path]; else c.projects[n.path] = meta;
    });

  const addBase = async (n: TreeNode, kind: "file" | "dir") => {
    let picked: string | string[] | null = null;
    try { picked = await pickPath({ directory: kind === "dir", multiple: false, defaultPath: n.path }); }
    catch (e) { setToast(`Não foi possível abrir o seletor: ${errorMessage(e)}`); return; }
    if (typeof picked !== "string") return;
    const path = picked;
    await updateConfig((c) => {
      const existing = c.bases.find((b) => b.path === path);
      if (existing) { if (!existing.appliesTo.includes(n.path)) existing.appliesTo.push(n.path); }
      else c.bases.push({ id: crypto.randomUUID(), path, label: null, appliesTo: [n.path], appliesToRegions: [] } satisfies ContextBase);
    });
  };
  const removeBaseFrom = (n: TreeNode, baseId: string) =>
    void updateConfig((c) => {
      const b = c.bases.find((x) => x.id === baseId);
      if (!b) return;
      b.appliesTo = b.appliesTo.filter((t) => t !== n.path);
      if (!b.appliesTo.length && !b.appliesToRegions.length) c.bases = c.bases.filter((x) => x.id !== baseId);
    });

  const deleteBase = (baseId: string) => void updateConfig((c) => { c.bases = c.bases.filter((x) => x.id !== baseId); });

  // Bases efetivas (resolvidas pelo backend, que é quem as injeta no launch).
  useEffect(() => {
    setResolved(null);
    if (!node) return;
    let cancelled = false;
    api.resolveBases(node.path)
      .then((r) => { if (!cancelled) setResolved(r); })
      .catch(() => { if (!cancelled) setResolved({ bases: [], skipped: [] }); });
    return () => { cancelled = true; };
  }, [node?.path, config?.bases]); // eslint-disable-line react-hooks/exhaustive-deps

  // Arestas de consulta: alvo → âncora da base, sempre em nós visíveis (sobe até o ancestral visível).
  const consult = useMemo<ConsultLink[]>(() => {
    if (!config?.bases.length || !tree) return [];
    const vis = new Set(visible.map((n) => n.id));
    const up = (n: TreeNode | null): string | null => {
      for (let cur: TreeNode | null | undefined = n; cur; cur = cur.parent ? idx.byId.get(cur.parent) : null) if (vis.has(cur.id)) return cur.id;
      return null;
    };
    const out = new Map<string, ConsultLink>();
    for (const b of config.bases) {
      const anchor = up(deepestNodeFor(idx, b.path));
      if (!anchor) continue;
      for (const t of b.appliesTo) {
        const from = up(deepestNodeFor(idx, t));
        if (from && from !== anchor) out.set(`${from}>${anchor}`, { from, to: anchor, isDir: b.path.endsWith("/") || !!idx.byId.get(b.path) });
      }
    }
    return [...out.values()];
  }, [config?.bases, tree, visible, idx]);

  // Abre a pasta na árvore: expande os ancestrais, seleciona e enquadra.
  const reveal = useCallback((id: string) => {
    setQuery("");
    setExpanded((prev) => { const next = new Set(prev); ancestors(idx, id).forEach((a) => next.add(a)); return next; });
    setSelected(id);
    requestFocus([id, ...(idx.byId.get(id)?.parent ? [idx.byId.get(id)?.parent as string] : [])]);
  }, [idx, requestFocus]);
  const paletteActions = useMemo<PaletteAction[]>(() => [
    { id: "settings", label: "Abrir Configurações", hint: "Ctrl+,", run: () => setShowSettings(true) },
    { id: "rescan", label: "Reescanear pastas", run: () => void load() },
    { id: "focus", label: "Alternar modo Foco", run: () => setFocusMode((v) => !v) },
    { id: "labels", label: "Alternar rótulos", run: () => setShowLabels((v) => !v) },
    { id: "color", label: "Alternar cor: categoria / pasta", run: () => changeColorMode(colorMode === "region" ? "folder" : "region") },
    { id: "collapse", label: "Recolher tudo", run: () => { const open = new Set(idx.roots); setExpanded(open); requestFocus(visibleNodes(idx, open, isHidden).map((n) => n.id)); } },
    { id: "fit", label: "Ajustar o grafo à tela", run: () => requestFocus(visible.map((n) => n.id)) },
  ], [load, idx, isHidden, visible, requestFocus, colorMode]); // eslint-disable-line react-hooks/exhaustive-deps

  const regionUse = useMemo(() => {
    const m = new Map<string, number>();
    Object.values(config?.projects ?? {}).forEach((p) => { if (p.region) m.set(p.region, (m.get(p.region) ?? 0) + 1); });
    return m;
  }, [config]);
  // Categoria efetiva de um caminho qualquer (para rotular a origem das bases na matriz).
  const sourceOf = useCallback((path: string): Region | null => {
    // A cópia dos agentes fica fora das raízes (dot-dir): conta como a categoria "agentes".
    if (agentsDir && isUnder(path, agentsDir)) return regionById.get("agentes") ?? null;
    const n = deepestNodeFor(idx, path);
    const id = n ? regionOf.get(n.id) : null;
    return id ? regionById.get(id) ?? null : null;
  }, [idx, regionOf, regionById, agentsDir]);
  const saveSettings = async (next: Config) => {
    const rescan = JSON.stringify([next.roots, next.ignore, next.treeMaxDepth]) !== JSON.stringify([config?.roots, config?.ignore, config?.treeMaxDepth]);
    await api.saveConfig(next);
    setConfig(next);
    if (rescan) void load();
  };

  const win = getCurrentWindow();
  const chosen = sessionId ?? sessions?.sessions[0]?.id ?? null;
  const total = tree?.nodes.length ?? 0;
  const projects = tree?.nodes.filter((n) => n.isProject).length ?? 0;

  return (
    <div className="window">
      <header className="titlebar" data-tauri-drag-region>
        <div className="traffic">
          <button aria-label="Fechar" className="tl red" onClick={() => void win.close()} />
          <button aria-label="Minimizar" className="tl yellow" onClick={() => void win.minimize()} />
          <button aria-label="Maximizar" className="tl green" onClick={() => void win.toggleMaximize()} />
        </div>
        <span className="logo" data-tauri-drag-region>br<b>AI</b>n</span>
        <button className="gear" aria-label="Configurações (Ctrl+,)" title="Configurações (Ctrl+,)" onClick={() => setShowSettings(true)}>⚙</button>
      </header>

      <div className="body">
        <main className="canvas">
          <FolderGraph nodes={visible} idx={idx} expanded={effectiveExpanded} colorOf={colorOf} consult={consult} selected={selected}
            focusMode={focusMode} showLabels={showLabels} matching={matching} focusRequest={focusReq}
            insets={{ left: SIDEBAR_W, right: node ? DETAIL_W : 0 }} onSelect={(id) => select(id)} onToggle={toggle} />
          {loading && (
            <div className="overlay" role="status">
              <div className="loadbox"><span>Varrendo pastas…</span><div className="skel" /><div className="skel" /><div className="skel" /></div>
            </div>
          )}
          {!loading && !total && !error && <div className="overlay"><div className="emptycard">Nenhuma pasta encontrada nas raízes configuradas.<br /><small>Adicione uma raiz em Configurações (Ctrl+,).</small></div></div>}
          <div className="legend" style={{ left: SIDEBAR_W + 16 }}>
            {groups.map((t) => (
              <div key={t.key} className={hiddenGroups.has(t.key) ? "off" : ""}>
                <i style={{ background: t.color }} />{t.name}<span>{t.count}</span>
              </div>
            ))}
          </div>
          <div className="actions" style={{ left: `calc(50% + ${(SIDEBAR_W - (node ? DETAIL_W : 0)) / 2}px)` }}>
            <button className={showLabels ? "on" : ""} onClick={() => setShowLabels((v) => !v)}>Labels</button>
            <button className={focusMode ? "on" : ""} onClick={() => setFocusMode((v) => !v)} title="Esmaece o que não é caminho nem vizinhança da seleção">Foco</button>
            <button onClick={() => changeColorMode(colorMode === "region" ? "folder" : "region")} title="Alterna a cor dos nós entre categoria e pasta de topo">Cor: {colorMode === "region" ? "categoria" : "pasta"}</button>
            <button onClick={() => { const open = new Set(idx.roots); setExpanded(open); requestFocus(visibleNodes(idx, open, isHidden).map((n) => n.id)); }}>Recolher</button>
            <button onClick={() => requestFocus(visible.map((n) => n.id))}>Ajustar</button>
          </div>
          {tree && (tree.warnings.length > 0 || tree.truncated) && (
            <div className="warn" style={{ left: SIDEBAR_W + 16 }} title={tree.warnings.join("\n")}>
              {tree.truncated ? "Árvore truncada · " : ""}{tree.warnings.length} aviso(s)
            </div>
          )}
        </main>

        <aside className="sidebar">
          <input ref={searchRef} className="search" placeholder="Filtrar pastas…" value={query}
            onChange={(e) => setQuery(e.target.value)} aria-label="Buscar pastas" />
          <div className="chips">
            {groups.map((t) => (
              <button key={t.key} className={`chip tip below ${hiddenGroups.has(t.key) ? "" : "on"}`} data-tip={t.desc ? `${t.name}: ${t.desc}` : undefined} onClick={() => toggleGroup(t.key)}>
                <i style={{ background: t.color }} />{t.name} <span>{t.count}</span>
              </button>
            ))}
          </div>
          <div className="list" role="tree" onKeyDown={onTreeKey}>
            {visible.map((n) => {
              const open = effectiveExpanded.has(n.id);
              return (
                <div key={n.id} className={`row ${selected === n.id ? "sel" : ""} ${matching?.has(n.id) ? "hit" : ""}`}
                  style={{ paddingLeft: 4 + n.depth * 12 }} role="treeitem" aria-expanded={n.childCount ? open : undefined}
                  aria-selected={selected === n.id} tabIndex={selected === n.id || (!selected && n.depth === 0) ? 0 : -1}
                  title={n.path} onClick={() => select(n.id, true)} onDoubleClick={() => toggle(n.id)}>
                  <span className={`chev ${n.childCount ? "" : "none"}`}
                    onClick={(e) => { e.stopPropagation(); toggle(n.id); }}>{n.childCount ? (open ? "▾" : "▸") : ""}</span>
                  <i className={n.isProject ? "" : "ring"} style={{ background: n.isProject ? colorOf(n) : "transparent", borderColor: colorOf(n) }} />
                  <span className="name">{n.name}</span>
                  {n.subtreeSessions > 0 && <span className="count" title="pensamentos nesta pasta e abaixo">{n.subtreeSessions}</span>}
                  <span className="when">{relativeTime(n.lastActivityMs)}</span>
                </div>
              );
            })}
          </div>
          <footer>
            <button onClick={() => void load()}>⟳ Reescanear</button>
            <span>{total} pastas · {projects} projetos</span>
          </footer>
        </aside>

        {node && (
          <aside className="detail">
            <button className="close" aria-label="Fechar painel" onClick={() => setSelected(null)}>✕</button>
            <p className="kind"><i style={{ background: colorOf(node) }} />{node.isProject ? "Projeto" : "Pasta"}{node.topLevel ? ` · ${node.topLevel}` : ""}</p>
            <h2>{node.name}</h2>
            <p className="mono path" title={node.path}>{node.path}</p>
            <p className="meta">⎇ {node.gitBranch ?? "— sem git"} · {relativeTime(node.lastActivityMs)}</p>
            <p className="meta">{node.childCount} subpastas · {node.descendantCount} no total</p>
            <div className="btns">
              <button className="primary" title="Abre o Claude Code" onClick={() => void launch({ kind: "new" })}>✦ Novo pensamento</button>
              <button disabled={!chosen} title="Abre o Claude Code na sessão selecionada" onClick={() => chosen && void launch({ kind: "resume", sessionId: chosen })}>↻ Retomar pensamento</button>
            </div>
            {node.childCount > 0 && (
              <div className="btns one">
                <button onClick={() => toggle(node.id)}>{effectiveExpanded.has(node.id) ? "Recolher subpastas" : "Expandir subpastas"}</button>
              </div>
            )}
            <h4>Categoria</h4>
            <div className="regionpick">
              <button className={`tip ${!ownRegion(node) ? "on" : ""}`} data-tip="Herdar: usa a categoria da pasta-pai. As subpastas continuam herdando daqui." onClick={() => setRegion(node, null)}>
                Herdar{!ownRegion(node) && regionOf.get(node.id) ? ` (${regionById.get(regionOf.get(node.id) as string)?.name})` : ""}
              </button>
              {regions.map((r) => (
                <button key={r.id} className={`tip ${ownRegion(node) === r.id ? "on" : ""}`} data-tip={`${r.name}: ${r.description}`} aria-label={`${r.name}: ${r.description}`} onClick={() => setRegion(node, r.id)}>
                  <i style={{ background: r.color }} />{r.name}
                </button>
              ))}
            </div>
            <h4>Bases de contexto <span>{resolved?.bases.length ?? "…"}</span></h4>
            <p className="meta">Arquivos e pastas que o Claude consulta em caso de dúvida ao abrir aqui (valem também para as subpastas).</p>
            <div className="bases">
              {resolved?.bases.map((b) => {
                const direct = b.via === "path" && b.inheritedFrom === node.path;
                return (
                  <div key={b.id} className="base" title={b.path}>
                    <span className="glyph">{b.isDir ? "▣" : "▢"}</span>
                    <span className="bname">{b.label ?? baseName(b.path)}
                      <span className="meta mono">{b.path}</span>
                      <span className="meta">
                        <b className={`acc ${b.access}`}>{b.access === "read" ? "somente leitura" : "pode acrescentar"}</b>
                        {b.via === "region"
                          ? ` · vale para a categoria ${regionById.get(b.inheritedFrom)?.name ?? b.inheritedFrom}`
                          : !direct && <> · herdada de <a href="#" onClick={(e) => { e.preventDefault(); select(b.inheritedFrom, true); }}>{baseName(b.inheritedFrom)}</a></>}
                      </span>
                      {resolved.skipped.filter((x) => x.id === b.id).map((x) => <span key="w" className="meta warnline">⚠ {cleanReason(x.reason)}</span>)}
                    </span>
                    {direct && <button className="x" aria-label="Remover base" onClick={() => removeBaseFrom(node, b.id)}>✕</button>}
                  </div>
                );
              })}
              {resolved?.skipped.filter((b) => !resolved.bases.some((x) => x.id === b.id)).map((b) => (
                <div key={b.id} className="base skipped" title={b.path}>
                  <span className="glyph">⚠</span>
                  <span className="bname">{baseName(b.path)}
                    <span className="meta mono">{b.path}</span>
                    <span className="meta warnline">{cleanReason(b.reason)}</span>
                  </span>
                  <button className="x" aria-label="Remover base" title="Remove esta base de todas as pastas" onClick={() => deleteBase(b.id)}>✕</button>
                </div>
              ))}
            </div>
            <div className="btns">
              <button onClick={() => void addBase(node, "file")}>+ Arquivo…</button>
              <button onClick={() => void addBase(node, "dir")}>+ Pasta…</button>
            </div>
            <h4>Pensamentos nesta pasta <span>{sessions?.sessions.length ?? "…"}</span></h4>
            {sessions && sessions.sessions.length === 0 && (
              <p className="meta">
                Nenhum pensamento aqui{node.subtreeSessions > 0 ? `; há ${node.subtreeSessions} nas subpastas — expanda para vê-los.` : "."}
              </p>
            )}
            <div className="sessions">
              {!sessions && [0, 1, 2].map((i) => <div key={i} className="skel" aria-hidden="true" />)}
              {sessions?.sessions.map((s) => (
                <button key={s.id} className={`session ${chosen === s.id ? "sel" : ""}`}
                  onClick={() => setSessionId(s.id)} onDoubleClick={() => void launch({ kind: "resume", sessionId: s.id })}>
                  <span className="title">{s.title}</span>
                  <span className="meta">
                    {relativeTime(s.lastTimestampMs ?? s.fileModifiedMs)} · {s.messageCount} mensagens
                    {s.degraded && <> · <span className="partial" title="Parte do arquivo da sessão não pôde ser lida">⚠ parcial</span></>}
                    {s.active && <> · <span className="live">em andamento</span></>}
                  </span>
                </button>
              ))}
            </div>
            {sessions && sessions.unreadable.length > 0 && <p className="meta">{sessions.unreadable.length} sessão(ões) ilegível(is).</p>}
          </aside>
        )}
      </div>
      {showPalette && <Palette nodes={tree?.nodes ?? []} actions={paletteActions} colorOf={colorOf} onPick={reveal} onClose={() => setShowPalette(false)} />}
      {showSettings && config && <Settings config={config} regionUse={regionUse} sourceOf={sourceOf} onSave={saveSettings} onClose={() => setShowSettings(false)} />}
      {error && <div className="toast error" role="alert" onClick={() => setError(null)}>{error}</div>}
      {toast && <div className="toast" role="status">{toast}</div>}
    </div>
  );
}
