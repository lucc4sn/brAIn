import { useEffect, useRef } from "react";
import ForceGraph from "force-graph";
import type { TreeNode } from "../lib/api";
import { ancestors, nodeRadius, type TreeIndex } from "../lib/tree";

export interface FocusRequest { ids: string[]; nonce: number }
/** Aresta de consulta: `from` (alvo) consulta a base ancorada em `to`. Ambos são nós visíveis. */
export interface ConsultLink { from: string; to: string; isDir: boolean }

interface Props {
  nodes: TreeNode[]; // só os visíveis
  idx: TreeIndex;
  expanded: Set<string>;
  colorOf: (n: TreeNode) => string;
  consult: ConsultLink[];
  selected: string | null;
  focusMode: boolean;
  showLabels: boolean;
  matching: Set<string> | null; // null = sem busca
  focusRequest: FocusRequest | null;
  insets: { left: number; right: number }; // áreas cobertas por sidebar/painel
  onSelect: (id: string | null) => void;
  onToggle: (id: string) => void;
}

interface GNode {
  id: string; name: string; color: string; r: number; depth: number;
  isProject: boolean; hiddenKids: number; x?: number; y?: number; fx?: number; fy?: number;
}
interface GLink { source: string | GNode; target: string | GNode }

const hex = (c: string, a: number) => {
  const n = parseInt(c.slice(1), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
};
const idOf = (e: string | GNode) => (typeof e === "string" ? e : e.id);

// Adaptador árvore de pastas -> force-graph (layout radial). Trocar a lib afeta só este arquivo.
export function FolderGraph(props: Props) {
  const { nodes, idx, expanded, colorOf, consult, selected, focusMode, showLabels, matching, focusRequest, insets } = props;
  const host = useRef<HTMLDivElement>(null);
  const graph = useRef<InstanceType<typeof ForceGraph> | null>(null);
  const cache = useRef(new Map<string, GNode>());
  const cb = useRef({ onSelect: props.onSelect, onToggle: props.onToggle });
  cb.current = { onSelect: props.onSelect, onToggle: props.onToggle };
  const live = useRef({
    selected, showLabels, matching, expanded, hover: null as string | null,
    focus: null as Set<string> | null, insets, consult,
  });
  live.current.selected = selected;
  live.current.showLabels = showLabels;
  live.current.matching = matching;
  live.current.expanded = expanded;
  live.current.insets = insets;
  live.current.consult = consult;
  const pendingFit = useRef<string[] | null>(null);

  // Conjunto em foco: caminho até a raiz + seleção + filhos visíveis da seleção.
  useEffect(() => {
    if (!focusMode || !selected) { live.current.focus = null; return; }
    const f = new Set<string>([selected, ...ancestors(idx, selected)]);
    if (expanded.has(selected)) (idx.children.get(selected) ?? []).forEach((c) => f.add(c));
    live.current.focus = f;
  }, [focusMode, selected, idx, expanded]);

  // Reaplicar uma propriedade (mesmo valor) marca o canvas como sujo para o próximo quadro.
  const redraw = () => { const g = graph.current; if (g) g.linkWidth(g.linkWidth()); };
  useEffect(redraw, [selected, focusMode, showLabels, matching, consult, insets, expanded, colorOf]);

  const fit = (ids: string[]) => {
    const g = graph.current; const el = host.current;
    if (!g || !el) return;
    const set = new Set(ids);
    const pts = [...cache.current.values()].filter((n) => set.has(n.id) && n.x !== undefined && n.y !== undefined);
    if (!pts.length) return;
    const xs = pts.map((p) => p.x as number); const ys = pts.map((p) => p.y as number);
    const [minX, maxX, minY, maxY] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    const pad = 80; const { left, right } = live.current.insets;
    const freeW = Math.max(120, el.clientWidth - left - right - 2 * pad);
    const freeH = Math.max(120, el.clientHeight - 2 * pad);
    const k = Math.max(0.3, Math.min(3.2, Math.min(freeW / Math.max(1, maxX - minX), freeH / Math.max(1, maxY - minY))));
    const shift = (left - right) / 2 / k; // centro da área livre, em coordenadas do mundo
    const ms = window.matchMedia("(prefers-reduced-motion: reduce)").matches ? 0 : 600;
    g.zoom(k, ms);
    g.centerAt((minX + maxX) / 2 - shift, (minY + maxY) / 2, ms);
  };

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    let lastClick = { id: "", t: 0 };
    const g = new ForceGraph(el)
      .backgroundColor("rgba(0,0,0,0)")
      .autoPauseRedraw(true) // ocioso = sem redesenho; mudanças externas pedem `redraw()`
      .dagMode("radialout")
      .dagLevelDistance(78)
      .d3VelocityDecay(0.4)
      .d3AlphaDecay(0.05)
      .nodeRelSize(1)
      .linkColor((l) => {
        const lk = l as GLink; const t = cache.current.get(idOf(lk.target));
        const f = live.current.focus;
        const hot = !f || (f.has(idOf(lk.source)) && f.has(idOf(lk.target)));
        return hex(t?.color ?? "#9aa0ab", hot ? 0.5 : 0.12);
      })
      .linkWidth((l) => {
        const lk = l as GLink; const f = live.current.focus;
        return f && f.has(idOf(lk.source)) && f.has(idOf(lk.target)) ? 1.8 : 1;
      })
      .nodeCanvasObject((n, ctx, scale) => {
        const node = n as GNode & { x: number; y: number };
        const L = live.current;
        const dimSearch = L.matching !== null && !L.matching.has(node.id);
        const dimFocus = L.focus !== null && !L.focus.has(node.id);
        ctx.globalAlpha = dimSearch ? 0.15 : dimFocus ? 0.28 : 1;
        const isSel = L.selected === node.id;
        if (node.isProject || node.depth === 0) {
          ctx.shadowColor = node.color;
          ctx.shadowBlur = node.r * (L.hover === node.id ? 3.2 : 2.2);
          ctx.fillStyle = node.color;
          ctx.beginPath(); ctx.arc(node.x, node.y, node.r, 0, 2 * Math.PI); ctx.fill();
          ctx.shadowBlur = 0;
        } else {
          ctx.fillStyle = "#0b0b10";
          ctx.strokeStyle = node.color;
          ctx.lineWidth = 1.6;
          ctx.beginPath(); ctx.arc(node.x, node.y, node.r, 0, 2 * Math.PI); ctx.fill(); ctx.stroke();
        }
        if (node.hiddenKids > 0) { // recolhido: anel tracejado + contagem
          ctx.save();
          ctx.setLineDash([2.5, 2.5]); ctx.strokeStyle = hex(node.color, 0.8); ctx.lineWidth = 1;
          ctx.beginPath(); ctx.arc(node.x, node.y, node.r + 3.5, 0, 2 * Math.PI); ctx.stroke();
          ctx.restore();
          ctx.font = `600 ${9 / Math.max(scale, 0.6)}px Inter, Cantarell, sans-serif`;
          ctx.textAlign = "left"; ctx.fillStyle = hex(node.color, 0.95);
          ctx.fillText(`+${node.hiddenKids}`, node.x + node.r + 5, node.y - node.r * 0.4);
        }
        if (isSel) {
          ctx.strokeStyle = "#8da9c4"; ctx.lineWidth = 2 / scale;
          ctx.beginPath(); ctx.arc(node.x, node.y, node.r + (node.hiddenKids ? 7 : 4), 0, 2 * Math.PI); ctx.stroke();
        }
        const showLabel = isSel || L.hover === node.id ||
          (L.showLabels && (node.depth <= 1 || scale > 0.55 + 0.3 * node.depth));
        if (showLabel) {
          const fs = (node.depth === 0 ? 13 : 11) / Math.max(scale, 0.5);
          ctx.font = `${node.depth <= 1 ? 600 : 400} ${fs}px Inter, Cantarell, sans-serif`;
          ctx.textAlign = "center"; ctx.lineJoin = "round"; ctx.lineWidth = 3 / Math.max(scale, 0.5);
          ctx.strokeStyle = "rgba(7,7,10,0.9)";
          const y = node.y + node.r + 11 / Math.max(scale, 0.5);
          ctx.strokeText(node.name, node.x, y);
          ctx.fillStyle = isSel ? "#f2f2f7" : "#b4b4c0";
          ctx.fillText(node.name, node.x, y);
        }
        ctx.globalAlpha = 1;
      })
      .nodePointerAreaPaint((n, color, ctx) => {
        const node = n as GNode & { x: number; y: number };
        ctx.fillStyle = color; ctx.beginPath(); ctx.arc(node.x, node.y, node.r + 6, 0, 2 * Math.PI); ctx.fill();
      })
      .onRenderFramePost((ctx, scale) => {
        const L = live.current;
        if (!L.consult.length) return;
        ctx.save();
        ctx.setLineDash([4 / scale, 4 / scale]);
        ctx.lineWidth = 1.3 / scale;
        for (const c of L.consult) {
          const a = cache.current.get(c.from); const b = cache.current.get(c.to);
          if (!a || !b || a.x === undefined || a.y === undefined || b.x === undefined || b.y === undefined) continue;
          const dim = L.focus !== null && !(L.focus.has(c.from) || L.focus.has(c.to));
          ctx.globalAlpha = dim ? 0.12 : 0.75;
          ctx.strokeStyle = "#8da9c4";
          const mx = (a.x + b.x) / 2; const my = (a.y + b.y) / 2;
          const nx = -(b.y - a.y) * 0.18; const ny = (b.x - a.x) * 0.18; // curva leve, para não colidir com a árvore
          ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.quadraticCurveTo(mx + nx, my + ny, b.x, b.y); ctx.stroke();
          // losango na âncora da base (cheio = pasta, vazado = arquivo)
          const d = 5 / Math.max(scale, 0.7);
          ctx.setLineDash([]);
          ctx.beginPath();
          ctx.moveTo(b.x, b.y - b.r - d * 1.9); ctx.lineTo(b.x + d, b.y - b.r - d * 0.9);
          ctx.lineTo(b.x, b.y - b.r + d * 0.1); ctx.lineTo(b.x - d, b.y - b.r - d * 0.9); ctx.closePath();
          if (c.isDir) { ctx.fillStyle = "#8da9c4"; ctx.fill(); } else { ctx.stroke(); }
          ctx.setLineDash([4 / scale, 4 / scale]);
        }
        ctx.restore();
      })
      .onNodeHover((n) => {
        live.current.hover = n ? (n as GNode).id : null;
        el.style.cursor = n ? "pointer" : "default";
      })
      .onNodeClick((n) => {
        const id = (n as GNode).id; const now = Date.now();
        if (lastClick.id === id && now - lastClick.t < 350) { cb.current.onToggle(id); lastClick = { id: "", t: 0 }; }
        else { cb.current.onSelect(id); lastClick = { id, t: now }; }
      })
      .onBackgroundClick(() => cb.current.onSelect(null))
      .onEngineStop(() => {
        if (pendingFit.current) { fit(pendingFit.current); pendingFit.current = null; }
      });
    g.d3Force("charge")?.strength(-160);
    graph.current = g;
    const ro = new ResizeObserver(() => g.width(el.clientWidth).height(el.clientHeight));
    ro.observe(el);
    return () => { ro.disconnect(); g._destructor(); graph.current = null; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Dados: preserva a identidade dos nós (posições) e nasce novos nós junto do pai.
  useEffect(() => {
    const keep = new Set(nodes.map((n) => n.id));
    for (const id of [...cache.current.keys()]) if (!keep.has(id)) cache.current.delete(id);
    const gnodes: GNode[] = nodes.map((n) => {
      const hiddenKids = n.childCount > 0 && !expanded.has(n.id) ? n.childCount : 0;
      const base = { name: n.name, color: colorOf(n), r: nodeRadius(n), depth: n.depth, isProject: n.isProject, hiddenKids };
      const old = cache.current.get(n.id);
      const pin = n.depth === 0 ? { fx: 0, fy: 0 } : {}; // raiz fixa no centro: sem tremor ao expandir
      if (old) { Object.assign(old, base, pin); return old; }
      const parent = n.parent ? cache.current.get(n.parent) : undefined;
      const fresh: GNode = { id: n.id, ...base, ...pin };
      if (parent?.x !== undefined && parent.y !== undefined) {
        fresh.x = parent.x + (Math.random() - 0.5) * 6; fresh.y = parent.y + (Math.random() - 0.5) * 6;
      }
      cache.current.set(n.id, fresh);
      return fresh;
    });
    const links: GLink[] = nodes.filter((n) => n.parent && keep.has(n.parent)).map((n) => ({ source: n.parent as string, target: n.id }));
    graph.current?.graphData({ nodes: gnodes, links });
  }, [nodes, idx, expanded, colorOf]);

  // Pedido de enquadramento (zoom de foco).
  useEffect(() => {
    if (!focusRequest) return;
    pendingFit.current = focusRequest.ids;
    const t = setTimeout(() => fit(focusRequest.ids), 380);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest?.nonce]);

  return <div ref={host} className="graph-host" />;
}
