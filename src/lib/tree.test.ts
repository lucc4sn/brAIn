import { describe, expect, it } from "vitest";
import type { TreeNode } from "./api";
import { ancestors, baseName, buildIndex, colorFor, deepestNodeFor, effectiveRegions, isUnder, nodeRadius, subtreeIds, visibleNodes } from "./tree";

const mk = (path: string, parent: string | null, depth: number, extra: Partial<TreeNode> = {}): TreeNode => ({
  id: path, path, parent, name: baseName(path), depth, isProject: false, hasGit: false, hasClaudeMd: false, gitBranch: null,
  childCount: 0, descendantCount: 0, ownSessions: 0, subtreeSessions: 0, lastActivityMs: null, ownScore: 0, subtreeScore: 0,
  topLevel: path.split("/")[2] ?? "", ownRegion: null, ...extra,
});

// /w ─┬─ areas ─┬─ pessoal ── brain
//     │         └─ clientes
//     └─ docs
const nodes: TreeNode[] = [
  mk("/w", null, 0),
  mk("/w/areas", "/w", 1, { topLevel: "areas" }),
  mk("/w/areas/pessoal", "/w/areas", 2, { topLevel: "areas" }),
  mk("/w/areas/pessoal/brain", "/w/areas/pessoal", 3, { topLevel: "areas", isProject: true, ownScore: 10 }),
  mk("/w/areas/clientes", "/w/areas", 2, { topLevel: "areas" }),
  mk("/w/docs", "/w", 1, { topLevel: "docs" }),
];
const idx = buildIndex(nodes, ["/w"]);

describe("buildIndex", () => {
  it("indexa filhos na ordem do backend e coleta pastas de topo sem repetir", () => {
    expect(idx.children.get("/w")).toEqual(["/w/areas", "/w/docs"]);
    expect(idx.children.get("/w/areas")).toEqual(["/w/areas/pessoal", "/w/areas/clientes"]);
    expect(idx.topLevels).toEqual(["areas", "docs"]);
    expect(idx.roots).toEqual(["/w"]);
  });
  it("aceita árvore vazia", () => {
    const e = buildIndex([], []);
    expect(e.byId.size).toBe(0);
    expect(visibleNodes(e, new Set())).toEqual([]);
  });
});

describe("ancestors / subtreeIds", () => {
  it("sobe do pai até a raiz", () => {
    expect(ancestors(idx, "/w/areas/pessoal/brain")).toEqual(["/w/areas/pessoal", "/w/areas", "/w"]);
    expect(ancestors(idx, "/w")).toEqual([]);
    expect(ancestors(idx, "/nao/existe")).toEqual([]);
  });
  it("lista o nó e todos os descendentes", () => {
    expect(new Set(subtreeIds(idx, "/w/areas"))).toEqual(new Set(["/w/areas", "/w/areas/pessoal", "/w/areas/pessoal/brain", "/w/areas/clientes"]));
  });
});

describe("visibleNodes", () => {
  it("mostra só a raiz quando nada está expandido", () => {
    expect(visibleNodes(idx, new Set()).map((n) => n.id)).toEqual(["/w"]);
  });
  it("percorre em pré-ordem apenas os ramos expandidos", () => {
    const v = visibleNodes(idx, new Set(["/w", "/w/areas"])).map((n) => n.id);
    expect(v).toEqual(["/w", "/w/areas", "/w/areas/pessoal", "/w/areas/clientes", "/w/docs"]);
  });
  it("não mostra descendentes de um ramo oculto, mas nunca oculta a raiz", () => {
    const hideAreas = (n: TreeNode) => n.name === "areas" || n.depth === 0;
    const v = visibleNodes(idx, new Set(["/w", "/w/areas"]), hideAreas).map((n) => n.id);
    expect(v).toEqual(["/w", "/w/docs"]);
  });
  it("ignora ids expandidos que não existem mais", () => {
    expect(visibleNodes(idx, new Set(["/w", "/fantasma"])).map((n) => n.id)).toEqual(["/w", "/w/areas", "/w/docs"]);
  });
});

describe("isUnder / baseName", () => {
  it("compara por componente, nunca por prefixo de texto", () => {
    expect(isUnder("/a/b", "/a/b")).toBe(true);
    expect(isUnder("/a/b/c", "/a/b")).toBe(true);
    expect(isUnder("/a/bc", "/a/b")).toBe(false);
    expect(isUnder("/a/b", "/a/b/")).toBe(true);
  });
  it("baseName ignora barras finais", () => {
    expect(baseName("/a/b/")).toBe("b");
    expect(baseName("/a/b")).toBe("b");
    expect(baseName("/")).toBe("/");
  });
});

describe("effectiveRegions", () => {
  it("herda a região do ancestral mais próximo e deixa null sem região", () => {
    const own = (n: TreeNode) => ({ "/w/areas": "educacao", "/w/areas/pessoal/brain": "projetos" } as Record<string, string>)[n.id] ?? null;
    const r = effectiveRegions(nodes, own);
    expect(r.get("/w")).toBeNull();
    expect(r.get("/w/areas")).toBe("educacao");
    expect(r.get("/w/areas/pessoal")).toBe("educacao");
    expect(r.get("/w/areas/pessoal/brain")).toBe("projetos");
    expect(r.get("/w/areas/clientes")).toBe("educacao");
    expect(r.get("/w/docs")).toBeNull();
  });
});

describe("deepestNodeFor", () => {
  it("acha o nó exato e o ancestral mais profundo de um caminho fora da árvore", () => {
    expect(deepestNodeFor(idx, "/w/areas")?.id).toBe("/w/areas");
    expect(deepestNodeFor(idx, "/w/areas/pessoal/brain/src/x.ts")?.id).toBe("/w/areas/pessoal/brain");
    expect(deepestNodeFor(idx, "/outra/raiz/x")).toBeNull();
    expect(deepestNodeFor(idx, "")).toBeNull();
  });
});

describe("colorFor / nodeRadius", () => {
  it("raiz tem cor própria; demais ciclam a paleta pela pasta de topo", () => {
    const palette = ["#111111", "#222222"];
    expect(colorFor(idx, palette, nodes[0])).toBe("#c9ccd6");
    expect(colorFor(idx, palette, nodes[1])).toBe("#111111");
    expect(colorFor(idx, palette, nodes[5])).toBe("#222222");
    expect(colorFor(idx, [], nodes[1])).toBe("#9aa0ab");
  });
  it("raio: raiz fixa, projeto cresce com a atividade e tem teto", () => {
    expect(nodeRadius(nodes[0])).toBe(15);
    expect(nodeRadius(mk("/p", "/w", 1, { isProject: true, ownScore: 1e9 }))).toBe(16);
    expect(nodeRadius(mk("/q", "/w", 1, { subtreeScore: 1e9 }))).toBe(11);
    expect(nodeRadius(nodes[3])).toBeGreaterThan(nodeRadius(mk("/r", "/w", 1, { isProject: true, ownScore: 0 })));
  });
});
