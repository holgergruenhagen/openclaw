// State-directory lookup without initializing process-wide config paths.
import { AsyncLocalStorage } from "node:async_hooks";
import os from "node:os";
import path from "node:path";
import { isPromiseLike } from "@openclaw/normalization-core/promise-like";
import { resolveHomeRelativePath, resolveRequiredHomeDir } from "../infra/home-dir.js";

const migrationSelection = new AsyncLocalStorage<{
  home: string;
  stateDir: string;
  active: boolean;
}>();

export function resolveNewStateDir(homedir: () => string = resolveRequiredHomeDir): string {
  return path.join(homedir(), ".openclaw");
}

/** Doctor carries its prepared source root through reads, relocation, and rollback. */
export function withMigrationStateDir<T>(
  env: NodeJS.ProcessEnv,
  stateDir: string,
  run: () => T,
): T {
  const selected = { home: resolveRequiredHomeDir(env, os.homedir), stateDir, active: true };
  const revoke = () => {
    selected.active = false;
  };
  try {
    const value = migrationSelection.run(selected, run);
    if (isPromiseLike(value)) {
      void Promise.resolve(value).then(revoke, revoke);
    } else {
      revoke();
    }
    return value;
  } catch (error) {
    revoke();
    throw error;
  }
}

/**
 * State directory for mutable data (sessions, logs, caches).
 * Can be overridden via OPENCLAW_STATE_DIR.
 * Default: ~/.openclaw
 */
export function resolveStateDir(
  env: NodeJS.ProcessEnv = process.env,
  homedir: () => string = () => resolveRequiredHomeDir(env, os.homedir),
): string {
  const effectiveHomedir = () => resolveRequiredHomeDir(env, homedir);
  const override = env.OPENCLAW_STATE_DIR?.trim();
  if (override) {
    return resolveHomeRelativePath(override, { env, homedir: effectiveHomedir });
  }
  return resolveStateDirFromHome(env, effectiveHomedir);
}

/** Select a default state directory from the caller's already resolved home. */
export function resolveStateDirFromHome(
  _env: NodeJS.ProcessEnv,
  effectiveHomedir: () => string,
): string {
  const selected = migrationSelection.getStore();
  if (selected?.home !== effectiveHomedir()) {
    return resolveNewStateDir(effectiveHomedir);
  }
  if (!selected.active) {
    throw new Error("Doctor's prepared state-directory selection has expired.");
  }
  return selected.stateDir;
}
