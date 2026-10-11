import { useEffect, useMemo, useState } from "react";
import { open as pickPath } from "@tauri-apps/plugin-dialog";
import { api, errorMessage, type Config, type ContextBase, type Region, type SyncReport, type TerminalKind, type TerminalStatus } from "./lib/api";

type Section = "pastas" | "terminal" | "regioes" | "contexto";
const NAV: { id: Section; label: string }[] = [
  { id: "pastas", label: "Pastas" },
  { id: "terminal", label: "Terminal" },
  { id: "regioes", label: "Categorias" },
  { id: "contexto", label: "Contexto" },
];
const SWATCHES = ["#D98B6B", "#6FB0A8", "#D4B26A", "#86B38A", "#BE8FA0", "#9AA0AB", "#8DA9C4", "#C4A484", "#7F9BB3", "#B5838D", "#A3B18A", "#C9ADA7"];
const KIND_LABEL: Record<TerminalKind, string> = { ptyxis: "Ptyxis", gnomeTerminal: "GNOME Terminal", custom: "Personalizado" };

function luminance(hex: string): number {
  const ch = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * ch[0] + 0.7152 * ch[1] + 0.0722 * ch[2];
}
const isHex = (s: string) => /^#[0-9a-fA-F]{6}$/.test(s);
/** Contraste da cor sobre o fundo do canvas (#07070A). */
function contrast(hex: string): number {
  return (luminance(hex) + 0.05) / (luminance("#07070a") + 0.05);
}
/** Remove ids de categoria inexistentes das bases; bases sem nenhum alvo somem. */
function pruneBases(c: Config) {
  const ids = new Set(c.regions.map((r) => r.id));
  c.bases.forEach((b) => { b.appliesToRegions = b.appliesToRegions.filter((r) => ids.has(r)); });
  c.bases = c.bases.filter((b) => b.appliesTo.length > 0 || b.appliesToRegions.length > 0);
}
const lines = (s: string) => s.split("\n").map((l) => l.trim()).filter(Boolean);

interface Props {
  config: Config;
  regionUse: Map<string, number>;
  sourceOf: (path: string) => Region | null;
  onSave: (next: Config) => Promise<void>;
  onClose: () => void;
}

export function Settings({ config, regionUse, sourceOf, onSave, onClose }: Props) {
  const [draft, setDraft] = useState<Config>(() => structuredClone(config));
  const [section, setSection] = useState<Section>("pastas");
  const [terminals, setTerminals] = useState<TerminalStatus[]>([]);
  const [ignoreText, setIgnoreText] = useState(() => config.ignore.join("\n"));
  const [argsText, setArgsText] = useState(() => config.terminal.customArgs.join("\n"));
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [confirmReset, setConfirmReset] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [sync, setSync] = useState<SyncReport | null>(null);
  const [confirmDel, setConfirmDel] = useState<string | null>(null);

  useEffect(() => { api.detectTerminals().then(setTerminals).catch(() => setTerminals([])); }, []);

  const dirty = useMemo(() => {
    const cur = { ...draft, ignore: lines(ignoreText), terminal: { ...draft.terminal, customArgs: lines(argsText) } };
    return JSON.stringify(cur) !== JSON.stringify(config);
  }, [draft, ignoreText, argsText, config]);

  const close = () => { if (!dirty) onClose(); else setConfirmDiscard(true); };
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { e.stopPropagation(); close(); } };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  });

  const patch = (f: (c: Config) => void) => setDraft((d) => { const n = structuredClone(d); f(n); return n; });
  const setRegion = (i: number, p: Partial<Region>) => patch((c) => { Object.assign(c.regions[i], p); });

  const badColor = draft.regions.some((r) => !isHex(r.color));
  const emptyName = draft.regions.some((r) => !r.name.trim());
  const blocked = badColor || emptyName;

  const save = async () => {
    setSaving(true);
    setErr(null);
    const next: Config = structuredClone(draft);
    next.ignore = lines(ignoreText);
    next.terminal.customArgs = lines(argsText);
    next.terminal.customExecutable = next.terminal.customExecutable?.trim() || null;
    next.terminal.claudePath = next.terminal.claudePath?.trim() || null;
    next.regions.forEach((r) => { r.name = r.name.trim(); r.color = r.color.toUpperCase(); });
    pruneBases(next);
    try { await onSave(next); onClose(); }
    catch (e) { setErr(errorMessage(e)); }
    finally { setSaving(false); }
  };

  const addRoot = async () => {
    try {
      const p = await pickPath({ directory: true, multiple: false });
      if (typeof p === "string" && !draft.roots.includes(p)) patch((c) => { c.roots.push(p); });
    } catch (e) { setErr(errorMessage(e)); }
  };

  const removeRegion = (i: number) => {
    const id = draft.regions[i].id;
    patch((c) => {
      c.regions.splice(i, 1);
      Object.values(c.projects).forEach((m) => { if (m.region === id) m.region = null; });
      pruneBases(c);
    });
    setConfirmDel(null);
  };
  const move = (i: number, d: -1 | 1) => patch((c) => {
    const j = i + d;
    if (j < 0 || j >= c.regions.length) return;
    [c.regions[i], c.regions[j]] = [c.regions[j], c.regions[i]];
  });
  const resetRegions = async () => {
    try {
      const def = await api.defaultConfig();
      patch((c) => {
        const ids = new Set(def.regions.map((r) => r.id));
        Object.values(c.projects).forEach((m) => { if (m.region && !ids.has(m.region)) m.region = null; });
        c.regions = def.regions;
        pruneBases(c);
      });
      setConfirmReset(false);
    } catch (e) { setErr(errorMessage(e)); }
  };

  const consumers = draft.regions.filter((r) => r.kind === "consumer");
  const toggleBase = (b: ContextBase, regionId: string) => patch((c) => {
    const t = c.bases.find((x) => x.id === b.id);
    if (!t) return;
    t.appliesToRegions = t.appliesToRegions.includes(regionId) ? t.appliesToRegions.filter((r) => r !== regionId) : [...t.appliesToRegions, regionId];
  });
  const addSource = async (kind: "file" | "dir", defaultPath?: string) => {
    try {
      const p = await pickPath({ directory: kind === "dir", multiple: false, defaultPath });
      if (typeof p !== "string") return;
      patch((c) => { if (!c.bases.some((b) => b.path === p)) c.bases.push({ id: crypto.randomUUID(), path: p, label: null, appliesTo: [], appliesToRegions: [] }); });
    } catch (e) { setErr(errorMessage(e)); }
  };
  const addAgent = async () => {
    try {
      const dir = await api.agentsCopyDir();
      setSync(await api.syncAgents());
      await addSource("file", dir);
    } catch (e) { setErr(errorMessage(e)); }
  };
  const doSync = async () => { try { setSync(await api.syncAgents()); } catch (e) { setErr(errorMessage(e)); } };

  const status = (k: TerminalKind) => terminals.find((t) => t.kind === k);
  const custom = draft.terminal.kind === "custom";

  return (
    <div className="modal-back" onMouseDown={(e) => { if (e.target === e.currentTarget) close(); }}>
      <div className="modal" role="dialog" aria-modal="true" aria-label="Configurações">
        <nav className="modal-nav">
          <h3>Configurações</h3>
          {NAV.map((n) => (
            <button key={n.id} className={section === n.id ? "on" : ""} onClick={() => setSection(n.id)}>{n.label}</button>
          ))}
        </nav>
        <div className="modal-main">
          <div className="modal-body">
            {section === "pastas" && (
              <>
                <h4>Pastas de varredura</h4>
                <p className="meta">A árvore do grafo parte destas pastas. Alterações valem após salvar (a árvore é re-escaneada).</p>
                {draft.roots.map((r, i) => (
                  <div key={r} className="row-item">
                    <span className="mono grow" title={r}>{r}</span>
                    <button className="x" aria-label={`Remover ${r}`} disabled={draft.roots.length === 1} onClick={() => patch((c) => { c.roots.splice(i, 1); })}>✕</button>
                  </div>
                ))}
                <button className="ghost" onClick={() => void addRoot()}>+ Adicionar pasta…</button>
                <label className="field">Profundidade máxima da árvore (1–16)
                  <input type="number" min={1} max={16} value={draft.treeMaxDepth}
                    onChange={(e) => patch((c) => { c.treeMaxDepth = Math.min(16, Math.max(1, Number(e.target.value) || 1)); })} />
                </label>
                <label className="field">Pastas ignoradas (um nome por linha)
                  <textarea rows={6} value={ignoreText} onChange={(e) => setIgnoreText(e.target.value)} spellCheck={false} />
                </label>
              </>
            )}

            {section === "terminal" && (
              <>
                <h4>Terminal</h4>
                <p className="meta">Onde o brAIn abre o Claude Code. Nunca usamos shell: o comando é montado como lista de argumentos.</p>
                <div className="seg" role="radiogroup" aria-label="Terminal">
                  {(["ptyxis", "gnomeTerminal", "custom"] as TerminalKind[]).map((k) => {
                    const st = status(k);
                    const off = k !== "custom" && st && !st.found;
                    return (
                      <button key={k} role="radio" aria-checked={draft.terminal.kind === k} className={draft.terminal.kind === k ? "on" : ""}
                        title={off ? (k === "ptyxis" ? "Não encontrado. Instale: sudo dnf install ptyxis" : "Não encontrado. Instale: sudo dnf install gnome-terminal") : undefined}
                        onClick={() => patch((c) => { c.terminal.kind = k; })}>
                        {KIND_LABEL[k]}
                        {k !== "custom" && st && <span className={st.found ? "ok" : "bad"}>{st.found ? `✓ ${st.executable ?? ""}` : "não encontrado"}</span>}
                      </button>
                    );
                  })}
                </div>
                {custom && (
                  <>
                    <label className="field">Executável do terminal (caminho absoluto)
                      <input className="mono" value={draft.terminal.customExecutable ?? ""} placeholder="/usr/bin/meu-terminal"
                        onChange={(e) => patch((c) => { c.terminal.customExecutable = e.target.value; })} />
                    </label>
                    <label className="field">Argumentos (um por linha)
                      <textarea className="mono" rows={4} value={argsText} onChange={(e) => setArgsText(e.target.value)} spellCheck={false}
                        placeholder={"-e\n{command}"} />
                    </label>
                    <p className="meta">A linha <code>{"{command}"}</code> é substituída pelo restante do argv (<code>claude …</code>) e precisa existir. O diretório de trabalho é aplicado pelo brAIn.</p>
                  </>
                )}
                <label className="field">Executável do claude (vazio = detectar automaticamente)
                  <input className="mono" value={draft.terminal.claudePath ?? ""} placeholder="~/.local/bin/claude"
                    onChange={(e) => patch((c) => { c.terminal.claudePath = e.target.value; })} />
                </label>
              </>
            )}


            {section === "contexto" && (
              <>
                <h4>Contexto por categoria</h4>
                <p className="meta">Marque quais categorias consumidoras podem consultar cada item. O acesso (leitura ou acréscimo) vem da categoria da fonte. Itens sem nenhuma marca e sem pasta específica são removidos ao salvar.</p>
                {consumers.length === 0 && <p className="meta warnline">Nenhuma categoria consumidora definida.</p>}
                {draft.bases.length > 0 && (
                  <table className="matrix">
                    <thead><tr><th>Item</th>{consumers.map((r) => <th key={r.id} title={r.description}><i style={{ background: r.color }} />{r.name}</th>)}</tr></thead>
                    <tbody>
                      {draft.bases.map((b) => {
                        const src = sourceOf(b.path);
                        return (
                          <tr key={b.id}>
                            <td title={b.path}>
                              <span className="mono">{b.path.split("/").filter(Boolean).slice(-2).join("/")}</span>
                              <span className="meta">{src ? `${src.name} · ${src.kind === "source" ? (src.access === "read" ? "somente leitura" : "acrescenta") : "somente leitura"}` : "sem categoria · somente leitura"}
                                {b.appliesTo.length > 0 ? ` · +${b.appliesTo.length} pasta(s) específica(s)` : ""}
                                {b.appliesTo.length === 0 && b.appliesToRegions.length === 0 ? " · marque uma categoria ou será removido ao salvar" : ""}</span>
                            </td>
                            {consumers.map((r) => (
                              <td key={r.id}><input type="checkbox" aria-label={`${b.path} para ${r.name}`} checked={b.appliesToRegions.includes(r.id)} onChange={() => toggleBase(b, r.id)} /></td>
                            ))}
                            <td><button className="x" aria-label="Remover item" onClick={() => patch((c) => { c.bases = c.bases.filter((x) => x.id !== b.id); })}>✕</button></td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                )}
                {draft.bases.length === 0 && <p className="meta">Nenhum item ainda. Atribua pastas às categorias-fonte e adicione arquivos ou pastas abaixo.</p>}
                <div className="btns-inline">
                  <button className="ghost" onClick={() => void addSource("file")}>+ Arquivo…</button>
                  <button className="ghost" onClick={() => void addSource("dir")}>+ Pasta…</button>
                  <button className="ghost" onClick={() => void addAgent()} title="Escolhe um agente na cópia sincronizada (~/.claude_copy/agents)">+ Agente…</button>
                  <button className="ghost" onClick={() => void doSync()}>Sincronizar agentes</button>
                </div>
                {sync && (
                  <p className="meta">{sync.sourceMissing ? "Pasta de agentes de origem não encontrada." : `Agentes: ${sync.copied} copiado(s), ${sync.unchanged} em dia.`}
                    {sync.stale.length > 0 ? ` ${sync.stale.length} só na cópia (não apagados).` : ""}{sync.skipped.length > 0 ? ` ${sync.skipped.length} ignorado(s).` : ""}</p>
                )}
                <p className="meta">Os agentes são copiados de <code>~/.claude/agents</code> para <code>~/.claude_copy/agents</code> (nunca apaga) ao abrir o app e antes de cada lançamento.</p>
              </>
            )}

            {section === "regioes" && (
              <>
                <h4>Categorias</h4>
                <p className="meta">Cada categoria vale para a pasta escolhida e é herdada pelas subpastas. Fontes (Documentações, Agentes, Diário) alimentam as consumidoras na aba Contexto.</p>
                {draft.regions.map((r, i) => {
                  const low = isHex(r.color) && contrast(r.color) < 3;
                  const used = regionUse.get(r.id) ?? 0;
                  return (
                    <div key={r.id} className="region-edit">
                      <div className="region-top">
                        <input type="color" aria-label={`Cor de ${r.name}`} value={isHex(r.color) ? r.color.toLowerCase() : "#9aa0ab"}
                          onChange={(e) => setRegion(i, { color: e.target.value })} />
                        <input className={`mono hex ${isHex(r.color) ? "" : "invalid"}`} aria-label="Cor (hex)" value={r.color} maxLength={7}
                          onChange={(e) => setRegion(i, { color: e.target.value })} />
                        <input className={`grow ${r.name.trim() ? "" : "invalid"}`} aria-label="Nome da categoria" value={r.name}
                          onChange={(e) => setRegion(i, { name: e.target.value })} />
                        <span className="meta">{used} pasta(s)</span>
                        <button className="x" aria-label="Subir" disabled={i === 0} onClick={() => move(i, -1)}>↑</button>
                        <button className="x" aria-label="Descer" disabled={i === draft.regions.length - 1} onClick={() => move(i, 1)}>↓</button>
                        <button className="x" aria-label={`Remover ${r.name}`} disabled={draft.regions.length === 1}
                          onClick={() => (used > 0 ? setConfirmDel(r.id) : removeRegion(i))}>✕</button>
                      </div>
                      <div className="swatches">
                        {SWATCHES.map((c) => (
                          <button key={c} type="button" aria-label={`Usar ${c}`} className={r.color.toUpperCase() === c ? "on" : ""} style={{ background: c }}
                            onClick={() => setRegion(i, { color: c })} />
                        ))}
                      </div>
                      <input aria-label="Descrição" placeholder="Descrição (aparece no tooltip)" value={r.description}
                        onChange={(e) => setRegion(i, { description: e.target.value })} />
                      <div className="kindrow">
                        <label>Papel
                          <select value={r.kind} onChange={(e) => setRegion(i, { kind: e.target.value as Region["kind"] })}>
                            <option value="consumer">Consumidora (abre o Claude aqui)</option>
                            <option value="source">Fonte (o Claude consulta)</option>
                          </select>
                        </label>
                        {r.kind === "source" && (
                          <label>Acesso
                            <select value={r.access} onChange={(e) => setRegion(i, { access: e.target.value as Region["access"] })}>
                              <option value="read">Somente leitura</option>
                              <option value="append">Pode acrescentar (nunca apagar)</option>
                            </select>
                          </label>
                        )}
                      </div>
                      {low && <p className="meta warnline">Contraste baixo ({contrast(r.color).toFixed(1)}:1) sobre o fundo do grafo; recomendado ≥ 3:1.</p>}
                      {confirmDel === r.id && (
                        <p className="meta warnline">
                          {used} pasta(s) usam esta categoria e voltarão a herdar da pasta-pai.{" "}
                          <button className="link" onClick={() => removeRegion(i)}>Remover mesmo assim</button>{" "}
                          <button className="link" onClick={() => setConfirmDel(null)}>Cancelar</button>
                        </p>
                      )}
                    </div>
                  );
                })}
                <div className="btns-inline">
                  <button className="ghost" onClick={() => patch((c) => { c.regions.push({ id: `r-${crypto.randomUUID().slice(0, 8)}`, name: "Nova categoria", color: "#8DA9C4", description: "", kind: "consumer", access: "read" }); })}>+ Adicionar categoria</button>
                  {!confirmReset
                    ? <button className="ghost" onClick={() => setConfirmReset(true)}>Restaurar categorias padrão</button>
                    : <span className="meta warnline">Substitui todas as categorias; pastas com categoria removida voltam a herdar.{" "}
                        <button className="link" onClick={() => void resetRegions()}>Confirmar</button>{" "}
                        <button className="link" onClick={() => setConfirmReset(false)}>Cancelar</button></span>}
                </div>
              </>
            )}
          </div>
          <footer className="modal-foot">
            <span className={`meta ${err ? "warnline" : ""}`} role={err ? "alert" : undefined}>
              {err ?? (blocked ? "Corrija os campos em vermelho para salvar." : dirty ? "Alterações não salvas" : "Sem alterações")}
            </span>
            {confirmDiscard
              ? <><button className="ghost" onClick={onClose}>Descartar alterações</button><button className="ghost" onClick={() => setConfirmDiscard(false)}>Continuar editando</button></>
              : <button className="ghost" onClick={close}>Cancelar</button>}
            <button className="primary" disabled={!dirty || blocked || saving} onClick={() => void save()}>{saving ? "Salvando…" : "Salvar"}</button>
          </footer>
        </div>
      </div>
    </div>
  );
}
