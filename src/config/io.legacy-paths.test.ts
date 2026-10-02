import fs from "node:fs/promises";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useAutoCleanupTempDirTracker } from "../../test/helpers/temp-dir.js";
import { createConfigIO } from "./io.factory.js";

const tempDirs = useAutoCleanupTempDirTracker(afterEach);

describe("canonical config admission", () => {
  it.each([
    { directory: ".clawdbot", filename: "clawdbot.json", explicitState: false },
    { directory: ".clawdbot", filename: "openclaw.json", explicitState: false },
    { directory: ".openclaw", filename: "clawdbot.json", explicitState: false },
    { directory: "selected", filename: "clawdbot.json", explicitState: true },
  ])(
    "preserves $directory/$filename until Doctor migrates it",
    async ({ directory, filename, explicitState }) => {
      const home = tempDirs.make("openclaw-canonical-path-admission-");
      const sourceDir = path.join(home, directory);
      const source = path.join(sourceDir, filename);
      const raw = '{"gateway":{"mode":"local","port":23941}}\n';
      await fs.mkdir(sourceDir);
      await fs.writeFile(source, raw);
      const io = createConfigIO({
        env: {
          HOME: home,
          OPENCLAW_HOME: home,
          ...(explicitState ? { OPENCLAW_STATE_DIR: sourceDir } : {}),
        },
        observe: false,
        pluginValidation: "core-only",
        logger: { error: vi.fn(), warn: vi.fn() },
      });

      expect(() => io.loadConfig()).toThrow("openclaw doctor --fix");
      await expect(io.readConfigFileSnapshot()).rejects.toThrow("openclaw doctor --fix");
      await expect(fs.readFile(source, "utf8")).resolves.toBe(raw);
      await expect(fs.stat(io.configPath)).rejects.toMatchObject({ code: "ENOENT" });
    },
  );

  it.each(["absent", "empty", "incidental"])(
    "does not admit state-only legacy homes (canonical: %s)",
    async (canonicalState) => {
      const home = tempDirs.make("openclaw-canonical-state-admission-");
      const legacy = path.join(home, ".clawdbot");
      await fs.mkdir(legacy);
      await fs.writeFile(path.join(legacy, "retained-state.txt"), "synthetic retained state\n");
      const canonical = path.join(home, ".openclaw");
      if (canonicalState !== "absent") {
        await fs.mkdir(canonical);
      }
      if (canonicalState === "incidental") {
        await fs.writeFile(path.join(canonical, ".env"), "SYNTHETIC_PATH_PROBE=retained\n");
      }
      const io = createConfigIO({
        env: { HOME: home, OPENCLAW_HOME: home },
        observe: false,
        pluginValidation: "core-only",
      });

      await expect(io.readConfigFileSnapshot()).rejects.toThrow("openclaw doctor --fix");
      if (canonicalState !== "absent") {
        await expect(fs.readdir(canonical)).resolves.toEqual(
          canonicalState === "incidental" ? [".env"] : [],
        );
      } else {
        await expect(fs.stat(canonical)).rejects.toMatchObject({ code: "ENOENT" });
      }
      await expect(fs.readFile(path.join(legacy, "retained-state.txt"), "utf8")).resolves.toBe(
        "synthetic retained state\n",
      );
    },
  );

  it("honors explicitly selected state and config paths with either spelling", async () => {
    const home = tempDirs.make("openclaw-explicit-legacy-path-");
    const stateDir = path.join(home, ".clawdbot");
    const configPath = path.join(stateDir, "clawdbot.json");
    await fs.mkdir(stateDir);
    await fs.writeFile(configPath, '{"gateway":{"mode":"local","port":23941}}\n');
    const io = createConfigIO({
      env: {
        HOME: home,
        OPENCLAW_HOME: home,
        OPENCLAW_STATE_DIR: stateDir,
        OPENCLAW_CONFIG_PATH: configPath,
      },
      observe: false,
      pluginValidation: "core-only",
    });

    const snapshot = await io.readConfigFileSnapshot();
    expect(snapshot.valid).toBe(true);
    expect(snapshot.path).toBe(configPath);
    expect(snapshot.config.gateway?.port).toBe(23941);
    await expect(fs.stat(path.join(home, ".openclaw"))).rejects.toMatchObject({ code: "ENOENT" });
  });

  it("admits completed tree aliases of canonical state without another migration", async () => {
    const home = tempDirs.make("openclaw-canonical-state-mirror-");
    const canonical = path.join(home, ".openclaw");
    const legacy = path.join(home, ".clawdbot");
    await fs.mkdir(canonical);
    await fs.mkdir(legacy);
    await fs.writeFile(path.join(canonical, "retained-state.txt"), "synthetic retained state\n");
    await fs.symlink(
      path.join(canonical, "retained-state.txt"),
      path.join(legacy, "retained-state.txt"),
      "file",
    );
    const io = createConfigIO({
      env: { HOME: home, OPENCLAW_HOME: home },
      observe: false,
      pluginValidation: "core-only",
    });

    const snapshot = await io.readConfigFileSnapshot();

    expect(snapshot.valid).toBe(true);
    expect(snapshot.path).toBe(path.join(canonical, "openclaw.json"));
    await expect(fs.readFile(path.join(legacy, "retained-state.txt"), "utf8")).resolves.toBe(
      "synthetic retained state\n",
    );
    await expect(fs.stat(snapshot.path)).rejects.toMatchObject({ code: "ENOENT" });
  });
});
