// Copies the roll CLI into resources/bin for electron-builder: `ROLL_BIN`, else the workspace's
// target/release/roll. Packaging without the CLI would ship an app that can't run agents, so a
// missing binary fails the build.
import { chmodSync, copyFileSync, existsSync, mkdirSync } from "node:fs";

const exe = process.platform === "win32" ? "roll.exe" : "roll";
const from = process.env.ROLL_BIN ?? new URL(`../../../target/release/${exe}`, import.meta.url).pathname;
if (!existsSync(from)) {
  console.error(`roll CLI not found at ${from}: run \`cargo build --release -p roll-cli\` or set ROLL_BIN`);
  process.exit(1);
}
const dir = new URL("../resources/bin/", import.meta.url);
mkdirSync(dir, { recursive: true });
const to = new URL(exe, dir).pathname;
copyFileSync(from, to);
chmodSync(to, 0o755);
console.log(`staged ${from} -> ${to}`);
