// A failing legacy-config copy must surface, not silently leave doctor
// looking like a clean fresh install while the operator's config exists.
import fs from "node:fs/promises";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { withTempDir } from "../test-utils/temp-dir.js";
import { runDoctorConfigPreflight } from "./doctor-config-preflight.js";

const envKeys = ["HOME", "OPENCLAW_CONFIG_PATH", "OPENCLAW_STATE_DIR"] as const;

function setEnv(values: Partial<Record<(typeof envKeys)[number], string>>) {
  for (const key of envKeys) {
    vi.stubEnv(key, values[key]);
  }
}

afterEach(() => vi.unstubAllEnvs());

describe("doctor legacy config migration failures", () => {
  it.runIf(process.platform !== "win32" && process.getuid?.() !== 0)(
    "surfaces a copy failure instead of proceeding as a fresh install",
    async () => {
      await withTempDir("openclaw-doctor-legacy-copy-", async (home) => {
        const legacyDir = path.join(home, ".clawdbot");
        await fs.mkdir(legacyDir, { recursive: true });
        await fs.writeFile(path.join(legacyDir, "clawdbot.json"), "{}\n", "utf-8");
        const targetDir = path.join(home, "readonly-state");
        await fs.mkdir(targetDir, { recursive: true });
        await fs.chmod(targetDir, 0o555);
        setEnv({
          HOME: home,
          OPENCLAW_CONFIG_PATH: path.join(targetDir, "openclaw.json"),
          OPENCLAW_STATE_DIR: legacyDir,
        });

        try {
          await expect(
            runDoctorConfigPreflight({ migrateState: false, invalidConfigNote: false }),
          ).rejects.toThrow(/Failed to migrate legacy config/);
        } finally {
          await fs.chmod(targetDir, 0o755);
        }
      });
    },
  );

  it("migrates the legacy config and reports the change when the copy works", async () => {
    await withTempDir("openclaw-doctor-legacy-copy-", async (home) => {
      const legacyDir = path.join(home, ".clawdbot");
      await fs.mkdir(legacyDir, { recursive: true });
      await fs.writeFile(path.join(legacyDir, "clawdbot.json"), "{}\n", "utf-8");
      const targetPath = path.join(home, "state-root", "openclaw.json");
      setEnv({
        HOME: home,
        OPENCLAW_CONFIG_PATH: targetPath,
        OPENCLAW_STATE_DIR: legacyDir,
      });

      await runDoctorConfigPreflight({ migrateState: false, invalidConfigNote: false });

      await expect(fs.readFile(targetPath, "utf-8")).resolves.toBe("{}\n");
    });
  });

  it.each([".openclaw", "selected"])(
    "copies a legacy filename within the selected %s state root",
    async (directory) => {
      await withTempDir("openclaw-doctor-legacy-filename-", async (home) => {
        const stateDir = path.join(home, directory);
        const source = path.join(stateDir, "clawdbot.json");
        const target = path.join(stateDir, "openclaw.json");
        const raw = '{"gateway":{"mode":"local"}}\n';
        await fs.mkdir(stateDir);
        await fs.writeFile(source, raw);
        setEnv({
          HOME: home,
          ...(directory === "selected" ? { OPENCLAW_STATE_DIR: stateDir } : {}),
        });

        await runDoctorConfigPreflight({ migrateState: false, invalidConfigNote: false });

        await expect(fs.readFile(target, "utf8")).resolves.toBe(raw);
        await expect(fs.readFile(source, "utf8")).resolves.toBe(raw);
      });
    },
  );

  it("does not copy another installation's config into an explicitly selected state root", async () => {
    await withTempDir("openclaw-doctor-legacy-isolation-", async (home) => {
      const legacyDir = path.join(home, ".clawdbot");
      const source = path.join(legacyDir, "openclaw.json");
      const stateDir = path.join(home, "selected");
      const raw = '{"gateway":{"mode":"local","port":23941}}\n';
      await fs.mkdir(legacyDir);
      await fs.writeFile(source, raw);
      setEnv({ HOME: home, OPENCLAW_STATE_DIR: stateDir });

      await runDoctorConfigPreflight({ migrateState: false, invalidConfigNote: false });

      await expect(fs.stat(path.join(stateDir, "openclaw.json"))).rejects.toMatchObject({
        code: "ENOENT",
      });
      await expect(fs.readFile(source, "utf8")).resolves.toBe(raw);
    });
  });
});
