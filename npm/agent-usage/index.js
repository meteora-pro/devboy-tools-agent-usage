"use strict";

const path = require("path");
const fs = require("fs");

const pkg = require("./package.json");

exports.name = pkg.name;
exports.version = pkg.version;

/**
 * Resolves the path to the devboy-agent-usage binary.
 *
 * Resolution order:
 * 1. DEVBOY_AGENT_USAGE_BINARY_PATH environment variable
 * 2. Platform-specific npm package
 *
 * @returns {string} Absolute path to the binary
 * @throws {Error} If binary cannot be found
 */
exports.getBinaryPath = function getBinaryPath() {
  // 1. Check environment variable override
  const envPath = process.env.DEVBOY_AGENT_USAGE_BINARY_PATH;
  if (envPath) {
    if (!fs.existsSync(envPath)) {
      throw new Error(
        `DEVBOY_AGENT_USAGE_BINARY_PATH is set to "${envPath}" but file does not exist.`,
      );
    }
    return path.resolve(envPath);
  }

  // 2. Resolve from platform-specific package
  const platformPkg = `@devboy-tools/agent-usage-${process.platform}-${process.arch}`;
  const ext = process.platform === "win32" ? ".exe" : "";
  const binaryName = `devboy-agent-usage${ext}`;

  try {
    const pkgJsonPath = require.resolve(`${platformPkg}/package.json`);
    const binaryPath = path.join(path.dirname(pkgJsonPath), "bin", binaryName);
    if (fs.existsSync(binaryPath)) {
      return binaryPath;
    }
  } catch {
    // Package not installed
  }

  throw new Error(
    `devboy-agent-usage binary not found. No package ${platformPkg} installed.\n` +
      "Your platform might not be supported. " +
      "Set DEVBOY_AGENT_USAGE_BINARY_PATH to point to a binary, or install from source:\n" +
      "  cargo install --git https://github.com/meteora-pro/devboy-agent-usage.git",
  );
};
