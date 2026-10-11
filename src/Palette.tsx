import { useEffect, useMemo, useRef, useState } from "react";
import type { TreeNode } from "./lib/api";

export interface PaletteAction { id: string; label: string; hint?: string; run: () => void }

interface Props {
  nodes: TreeNode[];
  actions: PaletteAction[];
  colorOf: (n: TreeNode) => string;
  onPick: (id: string) => void;
  onClose: () => void;
}

const MAX = 40;

function rank(n: TreeNode, q: string): number {
  const name = n.name.toLowerCase();
  if (name === q) return 0;
  if (name.startsWith(q)) return 1;
  if (name.includes(q)) return 2;
  if (n.path.toLowerCase().includes(q)) return 3;
  return -1;
}

/** Paleta de comandos (Ctrl+K): ações e pastas, filtradas por texto. */
export function Palette({ nodes, actions, colorOf, onPick, onClose }: Props) {
  const [q, setQ] = useState("");
  const [cur, setCur] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    input.current?.focus();
    return () => { prev?.focus?.(); };
  }, []);

  type Item = { key: string; label: string; sub: string; color?: string; run: () => void };
  const items = useMemo<Item[]>(() => {
    const term = q.trim().toLowerCase();
    const acts: Item[] = actions
      .filter((a) => !term || a.label.toLowerCase().includes(term))
      .map((a) => ({ key: `a:${a.id}`, label: a.label, sub: a.hint ?? "Ação", run: a.run }));
    const found = term
      ? nodes.map((n) => ({ n, r: rank(n, term) })).filter((x) => x.r >= 0).sort((a, b) => a.r - b.r || a.n.depth - b.n.depth || a.n.name.localeCompare(b.n.name)).slice(0, MAX).map((x) => x.n)
      : [];
    const folders: Item[] = found.map((n) => ({ key: `n:${n.id}`, label: n.name, sub: n.path, color: colorOf(n), run: () => onPick(n.id) }));
    return [...folders, ...acts];
  }, [q, nodes, actions, colorOf, onPick]);

  useEffect(() => { setCur(0); }, [q]);
  useEffect(() => { listRef.current?.querySelector<HTMLElement>(".sel")?.scrollIntoView({ block: "nearest" }); }, [cur]);

  const choose = (i: number) => { const it = items[i]; if (it) { onClose(); it.run(); } };
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") { e.preventDefault(); setCur((c) => Math.min(items.length - 1, c + 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setCur((c) => Math.max(0, c - 1)); }
    else if (e.key === "Enter") { e.preventDefault(); choose(cur); }
    else if (e.key === "Tab") { e.preventDefault(); input.current?.focus(); }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onClose(); }
  };

  return (
    <div className="modal-back top" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="palette" role="dialog" aria-modal="true" aria-label="Paleta de comandos" onKeyDown={onKey}>
        <input ref={input} placeholder="Buscar pastas ou comandos…" value={q} onChange={(e) => setQ(e.target.value)}
          role="combobox" aria-expanded="true" aria-controls="palette-list" aria-activedescendant={items[cur] ? `pal-${cur}` : undefined} />
        <div id="palette-list" className="plist" role="listbox" ref={listRef}>
          {items.length === 0 && <p className="meta pempty">Nada encontrado.</p>}
          {items.map((it, i) => (
            <div key={it.key} id={`pal-${i}`} role="option" aria-selected={i === cur} className={`pitem ${i === cur ? "sel" : ""}`}
              onMouseMove={() => setCur(i)} onClick={() => choose(i)}>
              {it.color ? <i style={{ background: it.color }} /> : <i className="act" />}
              <span className="pl">{it.label}</span>
              <span className="meta ps" title={it.sub}>{it.sub}</span>
            </div>
          ))}
        </div>
        <div className="pfoot meta">↑↓ navegar · Enter abrir · Esc fechar</div>
      </div>
    </div>
  );
}
