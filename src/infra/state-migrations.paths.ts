import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { isWithinDir } from "@openclaw/fs-safe/path";
import { resolveNewStateDir, resolveStateDir } from "../config/state-dir.js";
import {
  resolveOpenClawStateSqlitePath,
  resolveQuarantineStorePath,
} from "../state/openclaw-state-db.paths.js";
import { resolveRequiredHomeDir, resolveUserPath } from "./home-dir.js";
import { StartupMaintenanceRequiredError } from "./startup-maintenance-required.js";

export function resolveLegacyStateDirs(
  homedir: () => string = () => resolveRequiredHomeDir(process.env, os.homedir),
): string[] {
  return [path.join(homedir(), ".clawdbot")];
}

export function resolveSymlinkTarget(linkPath: string): string | null {
  try {
    const target = fs.readlinkSync(linkPath);
    return path.resolve(path.dirname(linkPath), target);
  } catch {
    return null;
  }
}

function isLegacyTreeSymlinkMirror(currentDir: string, realTargetDir: string): boolean {
  try {
    const entries = fs.readdirSync(currentDir, { withFileTypes: true });
    if (entries.length === 0) {
      return false;
    }
    return entries.every((entry) => {
      const entryPath = path.join(currentDir, entry.name);
      const stat = fs.lstatSync(entryPath);
      if (stat.isSymbolicLink()) {
        const resolvedTarget = resolveSymlinkTarget(entryPath);
        return Boolean(
          resolvedTarget && isWithinDir(realTargetDir, fs.realpathSync(resolvedTarget)),
        );
      }
      return stat.isDirectory() && isLegacyTreeSymlinkMirror(entryPath, realTargetDir);
    });
  } catch {
    return false;
  }
}

export function isLegacyDirSymlinkMirror(legacyDir: string, targetDir: string): boolean {
  try {
    return isLegacyTreeSymlinkMirror(legacyDir, fs.realpathSync(targetDir));
  } catch {
    return false;
  }
}

/** Source selection belongs to Doctor; runtime defaults never inspect old names. */
export function resolveStateDirForMigration(
  env: NodeJS.ProcessEnv = process.env,
  homedir: () => string = os.homedir,
): string {
  const home = () => resolveRequiredHomeDir(env, homedir);
  const canonical = resolveNewStateDir(home);
  if (env.OPENCLAW_STATE_DIR?.trim()) {
    return resolveStateDir(env, homedir);
  }
  if (
    [
      ...["openclaw.json", "clawdbot.json"].map((name) => path.join(canonical, name)),
      resolveOpenClawStateSqlitePath({ OPENCLAW_STATE_DIR: canonical }),
      resolveQuarantineStorePath({ OPENCLAW_STATE_DIR: canonical }),
    ].some((candidate) => fs.existsSync(candidate))
  ) {
    return canonical;
  }
  const legacy = resolveLegacyStateDirs(home).find((candidate) => fs.existsSync(candidate));
  if (
    !legacy ||
    (fs.existsSync(canonical) &&
      (fs.realpathSync(legacy) === fs.realpathSync(canonical) ||
        isLegacyDirSymlinkMirror(legacy, canonical)))
  ) {
    return canonical;
  }
  return legacy;
}

/** Includes both filenames for Doctor copy and supported updater rollback discovery. */
export function resolveDefaultConfigCandidates(
  env: NodeJS.ProcessEnv = process.env,
  homedir: () => string = os.homedir,
): string[] {
  const home = () => resolveRequiredHomeDir(env, homedir);
  const explicit = env.OPENCLAW_CONFIG_PATH?.trim();
  if (explicit) {
    return [resolveUserPath(explicit, env, home)];
  }
  const stateOverride = env.OPENCLAW_STATE_DIR?.trim();
  return [
    ...(stateOverride ? [resolveUserPath(stateOverride, env, home)] : []),
    resolveNewStateDir(home),
    ...resolveLegacyStateDirs(home),
  ].flatMap((directory) => [
    path.join(directory, "openclaw.json"),
    path.join(directory, "clawdbot.json"),
  ]);
}

export function resolveConfigPathForMigration(
  env: NodeJS.ProcessEnv = process.env,
  homedir: () => string = os.homedir,
): string {
  const explicit = env.OPENCLAW_CONFIG_PATH?.trim();
  const candidates = env.OPENCLAW_STATE_DIR?.trim()
    ? ["openclaw.json", "clawdbot.json"].map((name) =>
        path.join(resolveStateDir(env, homedir), name),
      )
    : resolveDefaultConfigCandidates(env, homedir);
  return explicit
    ? resolveUserPath(explicit, env, homedir)
    : (candidates.find((candidate) => fs.existsSync(candidate)) ??
        path.join(resolveStateDirForMigration(env, homedir), "openclaw.json"));
}

export function resolveLegacyConfigMigrationSources(
  env: NodeJS.ProcessEnv = process.env,
): string[] {
  const home = () => resolveRequiredHomeDir(env, os.homedir);
  const selected = path.join(resolveStateDir(env), "clawdbot.json");
  if (env.OPENCLAW_STATE_DIR?.trim()) {
    return [selected];
  }
  return [
    selected,
    ...resolveLegacyStateDirs(home).flatMap((directory) => [
      path.join(directory, "openclaw.json"),
      path.join(directory, "clawdbot.json"),
    ]),
  ];
}

/** Missing canonical inputs must not be admitted as a fresh install over retained state. */
export function assertCanonicalStatePaths(params: {
  env: NodeJS.ProcessEnv;
  homedir?: () => string;
  configPath: string;
}): void {
  const { env, configPath } = params;
  const homedir = params.homedir ?? os.homedir;
  const stateDir = resolveStateDir(env, homedir);
  const sourceDir = resolveStateDirForMigration(env, homedir);
  const canonicalConfig = path.join(stateDir, "openclaw.json");
  const differentState =
    sourceDir !== stateDir &&
    !(fs.existsSync(stateDir) && fs.realpathSync(sourceDir) === fs.realpathSync(stateDir));
  const source = differentState
    ? sourceDir
    : !env.OPENCLAW_CONFIG_PATH?.trim() &&
        path.resolve(configPath) === path.resolve(canonicalConfig) &&
        !fs.existsSync(configPath)
      ? (env.OPENCLAW_STATE_DIR?.trim()
          ? [path.join(stateDir, "clawdbot.json")]
          : resolveDefaultConfigCandidates(env, homedir)
        ).find((candidate) => candidate !== configPath && fs.existsSync(candidate))
      : undefined;
  if (source) {
    throw new StartupMaintenanceRequiredError(
      "state-migrations",
      `Legacy OpenClaw state remains at ${source}. Run "openclaw doctor --fix" before starting with the canonical state path. The legacy files have not been changed.`,
    );
  }
}
