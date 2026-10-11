// Wrappers tipados dos comandos Tauri. Tipos espelham os structs Rust (serde camelCase).
import { invoke } from "@tauri-apps/api/core";

export type RegionKind = "consumer" | "source";
export type Access = "read" | "append";
export interface Region { id: string; name: string; color: string; description: string; kind: RegionKind; access: Access }
export type TerminalKind = "ptyxis" | "gnomeTerminal" | "custom";
export interface TerminalConfig {
  kind: TerminalKind;
  customExecutable: string | null;
  customArgs: string[];
  claudePath: string | null;
}
export interface ContextBase { id: string; path: string; label: string | null; appliesTo: string[]; appliesToRegions: string[] }
export interface ResolvedBase { id: string; path: string; isDir: boolean; label: string | null; inheritedFrom: string; access: Access; via: "path" | "region" }
export interface SyncReport { copied: number; unchanged: number; skipped: string[]; stale: string[]; sourceMissing: boolean }
export interface SkippedBase { id: string; path: string; reason: string }
export interface ResolvedBases { bases: ResolvedBase[]; skipped: SkippedBase[] }
export interface ProjectMeta {
  region: string | null; tags: string[]; dependsOn: string[]; hidden: boolean; displayName: string | null;
}
export interface Config {
  roots: string[]; maxDepth: number; treeMaxDepth: number; ignore: string[]; showOrphans: boolean;
  terminal: TerminalConfig; regions: Region[]; projects: Record<string, ProjectMeta>; bases: ContextBase[];
}
export interface LoadedConfig { config: Config; recovered: boolean; warnings?: string[] }

export interface TreeNode {
  id: string; path: string; parent: string | null; name: string; depth: number;
  isProject: boolean; hasGit: boolean; hasClaudeMd: boolean; gitBranch: string | null;
  childCount: number; descendantCount: number;
  ownSessions: number; subtreeSessions: number; lastActivityMs: number | null;
  ownScore: number; subtreeScore: number; topLevel: string; ownRegion: string | null;
}
export interface TreeResult { nodes: TreeNode[]; rootIds: string[]; warnings: string[]; truncated: boolean }

export interface SessionSummary {
  id: string; title: string; titleSource: string;
  firstTimestampMs: number | null; lastTimestampMs: number | null; fileModifiedMs: number;
  messageCount: number; cwd: string | null; gitBranch: string | null;
  degraded: boolean; skippedLines: number; sizeBytes: number; active: boolean;
}
export interface UnreadableSession { fileName: string; reason: string }
export interface SessionsResult { sessions: SessionSummary[]; degradedCount: number; unreadable: UnreadableSession[] }

export type LaunchMode = { kind: "new" } | { kind: "continue" } | { kind: "resume"; sessionId: string };
export interface LaunchOutcome { terminalUsed: TerminalKind; basesApplied: number; basesSkipped: string[] }
export interface TerminalStatus { kind: TerminalKind; executable: string | null; found: boolean }
export interface AppError { kind: string; message: string }

export const api = {
  getConfig: () => invoke<LoadedConfig>("get_config"),
  saveConfig: (config: Config) => invoke<void>("save_config", { config }),
  scanTree: () => invoke<TreeResult>("scan_tree"),
  resolveBases: (projectPath: string) => invoke<ResolvedBases>("resolve_bases", { projectPath }),
  listSessions: (projectPath: string) => invoke<SessionsResult>("list_sessions", { projectPath }),
  detectTerminals: () => invoke<TerminalStatus[]>("detect_terminals"),
  defaultConfig: () => invoke<Config>("default_config"),
  syncAgents: () => invoke<SyncReport>("sync_agents"),
  agentsCopyDir: () => invoke<string>("agents_copy_dir"),
  launch: (projectPath: string, mode: LaunchMode) => invoke<LaunchOutcome>("launch", { projectPath, mode }),
};

export function errorMessage(e: unknown): string {
  if (typeof e === "object" && e !== null && "message" in e) return String((e as AppError).message);
  return String(e);
}

export function relativeTime(ms: number | null, now = Date.now()): string {
  if (ms === null) return "—";
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return "agora";
  const m = Math.round(s / 60);
  if (m < 60) return `há ${m} min`;
  const h = Math.round(m / 60);
  if (h < 24) return `há ${h} h`;
  const d = Math.round(h / 24);
  if (d < 30) return `há ${d} d`;
  return `há ${Math.round(d / 30)} mês(es)`;
}
