// Índice e consultas sobre a árvore de pastas devolvida por `scan_tree`.
import type { TreeNode } from "./api";

export interface TreeIndex {
  byId: Map<string, TreeNode>;
  children: Map<string, string[]>; // ordem = ordem do backend (nome, case-insensitive)
  roots: string[];
  topLevels: string[]; // nomes das pastas de depth 1, na ordem em que aparecem
}

export function buildIndex(nodes: TreeNode[], rootIds: string[]): TreeIndex {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const children = new Map<string, string[]>();
  const topLevels: string[] = [];
  for (const n of nodes) {
    if (n.parent) {
      const list = children.get(n.parent);
      if (list) list.push(n.id); else children.set(n.parent, [n.id]);
    }
    if (n.depth === 1 && !topLevels.includes(n.name)) topLevels.push(n.name);
  }
  return { byId, children, roots: rootIds, topLevels };
}

/** Ancestrais do nó, do pai até a raiz. */
export function ancestors(idx: TreeIndex, id: string): string[] {
  const out: string[] = [];
  let cur = idx.byId.get(id)?.parent ?? null;
  while (cur) {
    out.push(cur);
    cur = idx.byId.get(cur)?.parent ?? null;
  }
  return out;
}

/** Nós visíveis em pré-ordem: raízes sempre; filhos só de nós expandidos e de ramos não ocultos. */
export function visibleNodes(idx: TreeIndex, expanded: Set<string>, isHidden: (n: TreeNode) => boolean = () => false): TreeNode[] {
  const out: TreeNode[] = [];
  const walk = (id: string) => {
    const n = idx.byId.get(id);
    if (!n || (n.depth >= 1 && isHidden(n))) return;
    out.push(n);
    if (expanded.has(id)) (idx.children.get(id) ?? []).forEach(walk);
  };
  idx.roots.forEach(walk);
  return out;
}

/** Ids de `id` e de todos os seus descendentes. */
export function subtreeIds(idx: TreeIndex, id: string): string[] {
  const out: string[] = [];
  const stack = [id];
  while (stack.length) {
    const cur = stack.pop() as string;
    out.push(cur);
    (idx.children.get(cur) ?? []).forEach((c) => stack.push(c));
  }
  return out;
}

export const ROOT_COLOR = "#c9ccd6";
export const FALLBACK_COLOR = "#9aa0ab";

/** Cor por pasta de topo, ciclando a paleta (as cores das regiões da config). */
export function colorFor(idx: TreeIndex, palette: string[], n: TreeNode): string {
  if (n.depth === 0) return ROOT_COLOR;
  const i = idx.topLevels.indexOf(n.topLevel);
  return palette.length && i >= 0 ? palette[i % palette.length] : FALLBACK_COLOR;
}

export function nodeRadius(n: TreeNode): number {
  if (n.depth === 0) return 15;
  if (n.isProject) return Math.min(16, 6.5 + Math.log1p(n.ownScore) * 2.6);
  return Math.min(11, 3.5 + Math.log1p(n.subtreeScore) * 1.6);
}

/** `p` é igual a `a` ou está abaixo dela (compara por componente, nunca por prefixo de string). */
export function isUnder(p: string, a: string): boolean {
  const base = a.replace(/\/+$/, "");
  return p === base || p.startsWith(`${base}/`);
}
export const baseName = (p: string) => p.replace(/\/+$/, "").split("/").pop() || p;

/** Região efetiva de cada nó: a própria ou a do ancestral mais próximo (null = sem região). */
export function effectiveRegions(nodes: TreeNode[], own: (n: TreeNode) => string | null): Map<string, string | null> {
  const out = new Map<string, string | null>();
  for (const n of nodes) out.set(n.id, own(n) ?? (n.parent ? out.get(n.parent) ?? null : null));
  return out;
}

/** Nó da árvore mais profundo que contém `path` (ou o próprio), ou null se fora de todas as raízes. */
export function deepestNodeFor(idx: TreeIndex, path: string): TreeNode | null {
  let cur = path;
  for (let guard = 0; guard < 64 && cur; guard++) {
    const n = idx.byId.get(cur);
    if (n) return n;
    const cut = cur.lastIndexOf("/");
    if (cut <= 0) return null;
    cur = cur.slice(0, cut);
  }
  return null;
}
