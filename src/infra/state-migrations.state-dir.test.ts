// Verifies state-dir migrations preserve existing OpenClaw runtime data.
import fs from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";
import {
  resolveOpenClawStateSqlitePath,
  resolveQuarantineStorePath,
} from "../state/openclaw-state-db.paths.js";
import { withTestDir } from "../test-helpers/temp-dir.js";
import {
  autoMigrateLegacyStateDir,
  prepareLegacyStateDirMigration,
  resetAutoMigrateLegacyStateDirForTest,
} from "./state-migrations.state-dir.js";

async function withStateDirFixture(run: (root: string) => Promise<void>): Promise<void> {
  try {
    await withTestDir({ prefix: "openclaw-state-dir-" }, async (root) => {
      await run(root);
    });
  } finally {
    resetAutoMigrateLegacyStateDirForTest();
  }
}

describe("legacy state dir auto-migration", () => {
  it.each([
    { location: "source", selector: "default" },
    { location: "source", selector: "environment" },
    { location: "source", selector: "config" },
    { location: "source", selector: "prefixed-config" },
    { location: "source", selector: "selected-config" },
    { location: "source", selector: "supplied-home-environment" },
    { location: "source", selector: "supplied-home-config" },
    { location: "target", selector: "default" },
    { location: "custom", selector: "default" },
  ])(
    "preserves retired OAuth sidecars before relocating $location with $selector selection",
    async ({ location, selector }) => {
      await withStateDirFixture(async (root) => {
        const legacyDir = path.join(root, ".clawdbot");
        const targetDir = path.join(root, ".openclaw");
        const stateDir =
          location === "custom"
            ? path.join(root, "custom-state")
            : location === "target"
              ? targetDir
              : legacyDir;
        const oauthDir = path.join(stateDir, selector === "default" ? "credentials" : "oauth$old");
        const sidecarPath = path.join(oauthDir, "auth-profiles", `${"a".repeat(32)}.json`);
        const sidecarBytes = Buffer.from("retired encrypted bytes\u0000not parsed\n");
        const suppliedHome = selector.startsWith("supplied-home-");
        const env: NodeJS.ProcessEnv = suppliedHome ? {} : { HOME: root, OPENCLAW_HOME: root };
        if (location === "custom") {
          env.OPENCLAW_STATE_DIR = stateDir;
        }
        if (selector === "environment" || selector === "supplied-home-environment") {
          env.OPENCLAW_OAUTH_DIR = suppliedHome ? "~/.clawdbot/oauth$old" : oauthDir;
        }
        const config =
          selector === "config" ||
          selector === "prefixed-config" ||
          selector === "selected-config" ||
          selector === "supplied-home-config"
            ? {
                env: {
                  vars: {
                    OPENCLAW_OAUTH_DIR:
                      selector === "supplied-home-config"
                        ? "${HOME}/.clawdbot/oauth$old"
                        : "~/.clawdbot/oauth$old",
                  },
                },
              }
            : {};
        fs.mkdirSync(legacyDir, { recursive: true });
        fs.mkdirSync(path.dirname(sidecarPath), { recursive: true });
        fs.writeFileSync(sidecarPath, sidecarBytes);
        const configPath =
          selector === "selected-config"
            ? path.join(root, "selected-config.json")
            : path.join(stateDir, "clawdbot.json");
        if (selector === "selected-config") {
          env.OPENCLAW_CONFIG_PATH = configPath;
        }
        const configBytes = `${selector === "prefixed-config" ? "unexpected prefix\n" : ""}${JSON.stringify(config)}`;
        fs.writeFileSync(configPath, configBytes);
        const envBefore = { ...env };

        for (let attempt = 0; attempt < 2; attempt += 1) {
          await expect(autoMigrateLegacyStateDir({ env, homedir: () => root })).rejects.toThrow(
            "Upgrade through OpenClaw 2026.9.7",
          );
          expect(fs.lstatSync(legacyDir).isSymbolicLink()).toBe(false);
          expect(fs.existsSync(targetDir)).toBe(location === "target");
          expect(fs.readFileSync(sidecarPath)).toEqual(sidecarBytes);
          expect(fs.readFileSync(configPath, "utf8")).toBe(configBytes);
          expect(env).toEqual(envBefore);
        }
      });
    },
  );

  it.each([
    { name: "primary", resolve: resolveOpenClawStateSqlitePath },
    { name: "quarantine", resolve: resolveQuarantineStorePath },
  ])(
    "keeps populated configless canonical $name state ahead of legacy residue",
    async ({ resolve }) => {
      await withStateDirFixture(async (root) => {
        const canonical = path.join(root, ".openclaw");
        const legacy = path.join(root, ".clawdbot");
        const databasePaths = [canonical, legacy].map((stateDir) =>
          resolve({ OPENCLAW_STATE_DIR: stateDir }),
        );
        for (const [index, pathname] of databasePaths.entries()) {
          fs.mkdirSync(path.dirname(pathname), { recursive: true });
          const db = new DatabaseSync(pathname);
          try {
            db.exec("CREATE TABLE retained_state (value TEXT NOT NULL)");
            db.prepare("INSERT INTO retained_state(value) VALUES (?)").run(
              index === 0 ? "current" : "older",
            );
          } finally {
            db.close();
          }
        }
        const before = databasePaths.map((pathname) => fs.readFileSync(pathname));

        const prepared = prepareLegacyStateDirMigration({ env: {}, homedir: () => root });

        expect(prepared?.stateDir).toBe(canonical);
        expect(prepared?.result.migrated).toBe(false);
        expect(databasePaths.map((pathname) => fs.readFileSync(pathname))).toEqual(before);
        expect(fs.lstatSync(legacy).isDirectory()).toBe(true);
      });
    },
  );

  it("skips a legacy symlinked state dir when it points outside supported legacy roots", async () => {
    await withStateDirFixture(async (root) => {
      const legacySymlink = path.join(root, ".clawdbot");
      const legacyDir = path.join(root, "legacy-state-source");

      fs.mkdirSync(legacyDir, { recursive: true });
      fs.writeFileSync(path.join(legacyDir, "marker.txt"), "ok", "utf-8");

      const dirLinkType = process.platform === "win32" ? "junction" : "dir";
      fs.symlinkSync(legacyDir, legacySymlink, dirLinkType);

      const result = await autoMigrateLegacyStateDir({
        env: {} as NodeJS.ProcessEnv,
        homedir: () => root,
      });

      expect(result.migrated).toBe(false);
      expect(result.warnings).toEqual([
        `Legacy state dir is a symlink (${legacySymlink} → ${legacyDir}); skipping auto-migration.`,
      ]);
      expect(fs.readFileSync(path.join(root, "legacy-state-source", "marker.txt"), "utf-8")).toBe(
        "ok",
      );
      expect(fs.readFileSync(path.join(root, ".clawdbot", "marker.txt"), "utf-8")).toBe("ok");
    });
  });

  it("links an empty legacy state dir to an existing canonical root", async () => {
    await withStateDirFixture(async (root) => {
      const legacyDir = path.join(root, ".clawdbot");
      const targetDir = path.join(root, ".openclaw");
      fs.mkdirSync(legacyDir, { recursive: true });
      fs.mkdirSync(targetDir, { recursive: true });
      fs.writeFileSync(path.join(targetDir, "openclaw.json"), "{}", "utf-8");

      const result = await autoMigrateLegacyStateDir({
        env: {} as NodeJS.ProcessEnv,
        homedir: () => root,
      });

      expect(result).toMatchObject({ migrated: true, skipped: false, warnings: [] });
      expect(result.changes).toContain(
        `State dir: ${legacyDir} → ${targetDir} (legacy path now symlinked)`,
      );
      expect(fs.realpathSync(legacyDir)).toBe(fs.realpathSync(targetDir));
    });
  });

  it("skips state-dir migration when OPENCLAW_STATE_DIR is explicitly set", async () => {
    await withStateDirFixture(async (root) => {
      const legacyDir = path.join(root, ".clawdbot");
      fs.mkdirSync(legacyDir, { recursive: true });

      const result = await autoMigrateLegacyStateDir({
        env: { OPENCLAW_STATE_DIR: path.join(root, "custom-state") } as NodeJS.ProcessEnv,
        homedir: () => root,
      });

      expect(result).toEqual({
        migrated: false,
        skipped: true,
        changes: [],
        warnings: [],
      });
      expect(fs.existsSync(legacyDir)).toBe(true);
    });
  });

  it.each(["custom", "canonical", "legacy"] as const)(
    "refuses pre-July plugin JSON without moving or changing the %s state root",
    async (location) => {
      await withStateDirFixture(async (root) => {
        const stateDir = path.join(
          root,
          location === "custom"
            ? "custom-state"
            : location === "legacy"
              ? ".clawdbot"
              : ".openclaw",
        );
        const sourcePath = path.join(stateDir, "plugins", "installs.json");
        const source = '{"records":{"demo":{"source":"npm","spec":"demo@1.0.0"}}}';
        fs.mkdirSync(path.dirname(sourcePath), { recursive: true });
        fs.writeFileSync(sourcePath, source);

        const result = await autoMigrateLegacyStateDir({
          env: location === "custom" ? { OPENCLAW_STATE_DIR: stateDir } : {},
          homedir: () => root,
        });

        expect(result).toMatchObject({ migrated: false, skipped: false, changes: [] });
        expect(result.warnings).toEqual([
          expect.stringContaining("Run openclaw doctor --fix on 2026.9.5 with a pre-update backup"),
        ]);
        expect(fs.readFileSync(sourcePath, "utf8")).toBe(source);
        expect(fs.existsSync(`${sourcePath}.migrated`)).toBe(false);
        expect(fs.existsSync(path.join(stateDir, "state", "openclaw.sqlite"))).toBe(false);
        expect(fs.lstatSync(stateDir).isSymbolicLink()).toBe(false);
        if (location === "legacy") {
          expect(fs.existsSync(path.join(root, ".openclaw"))).toBe(false);
        }
      });
    },
  );

  it.each(
    (["explicit", "legacy"] as const).flatMap((location) =>
      ["delivery-queue/pending.json", "session-delivery-queue/pending.json"].map(
        (relativePath) => ({ location, relativePath }),
      ),
    ),
  )(
    "refuses retired $relativePath in the $location state dir without relocation or archival",
    async ({ location, relativePath }) => {
      await withStateDirFixture(async (root) => {
        const legacyDir = path.join(root, ".clawdbot");
        const targetDir = path.join(root, ".openclaw");
        const stateDir = location === "explicit" ? path.join(root, "custom-state") : legacyDir;
        const sourcePath = path.join(stateDir, relativePath);
        const sourceBytes = Buffer.from('{"id":"retired","payloads":[{"text":"preserve"}]}\n');
        fs.mkdirSync(legacyDir, { recursive: true });
        fs.writeFileSync(path.join(legacyDir, "marker.txt"), "ok", "utf8");
        fs.mkdirSync(path.dirname(sourcePath), { recursive: true });
        fs.writeFileSync(sourcePath, sourceBytes);
        const params = {
          env: location === "explicit" ? { OPENCLAW_STATE_DIR: stateDir } : {},
          homedir: () => root,
        };

        for (let attempt = 0; attempt < 2; attempt += 1) {
          const migration = autoMigrateLegacyStateDir(params);
          await expect(migration).rejects.toMatchObject({
            message: expect.stringContaining(sourcePath),
          });
          await expect(migration).rejects.toThrow(/July 1, 2026.*OpenClaw 2026\.9\.7/);
          expect(fs.readFileSync(sourcePath)).toEqual(sourceBytes);
          expect(fs.readdirSync(path.dirname(sourcePath))).toEqual([path.basename(sourcePath)]);
          expect(fs.lstatSync(legacyDir).isSymbolicLink()).toBe(false);
          expect(fs.readFileSync(path.join(legacyDir, "marker.txt"), "utf8")).toBe("ok");
          expect(fs.existsSync(targetDir)).toBe(false);
        }

        fs.renameSync(sourcePath, path.join(root, "retired-source.backup.json"));
        const recovered = await autoMigrateLegacyStateDir(params);
        expect(recovered).toMatchObject({
          migrated: location === "legacy",
          skipped: location === "explicit",
          warnings: [],
        });
        if (location === "legacy") {
          expect(fs.realpathSync(legacyDir)).toBe(fs.realpathSync(targetDir));
          expect(fs.readFileSync(path.join(targetDir, "marker.txt"), "utf8")).toBe("ok");
        }
      });
    },
  );

  it("only runs once per process until reset", async () => {
    await withStateDirFixture(async (root) => {
      const legacyDir = path.join(root, ".clawdbot");
      fs.mkdirSync(legacyDir, { recursive: true });
      fs.writeFileSync(path.join(legacyDir, "marker.txt"), "ok", "utf-8");

      const first = await autoMigrateLegacyStateDir({
        env: {} as NodeJS.ProcessEnv,
        homedir: () => root,
      });
      const second = await autoMigrateLegacyStateDir({
        env: {} as NodeJS.ProcessEnv,
        homedir: () => root,
      });

      expect(first.migrated).toBe(true);
      expect(second).toEqual({
        migrated: false,
        skipped: true,
        changes: [],
        warnings: [],
      });
    });
  });
});
