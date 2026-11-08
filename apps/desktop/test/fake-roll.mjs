#!/usr/bin/env node
// A stand-in for the `roll` CLI in tests, printing what `roll agent login/up` print
// (crates/roll-cli/src/agent.rs). Behaviour comes from the environment:
//   FAKE_ROLL_LOG     file each invocation appends "<pid> <args…>" to
//   FAKE_ROLL_EXIT    `agent up`: exit with this code after FAKE_ROLL_RUN_MS (default: run until signalled)
//   FAKE_ROLL_RUN_MS  how long `agent up` runs before exiting
//   FAKE_ROLL_IGNORE_TERM  `agent up` ignores SIGTERM (tests the SIGKILL fallback)
//   FAKE_ROLL_LOGIN   `agent login`: "approve" writes FAKE_ROLL_AGENT_JSON and exits 0, "deny" exits 1
//   FAKE_ROLL_AGENT_JSON  where `agent login` writes agent.json
//   FAKE_ROLL_WEB     the web origin in the approve URL (default https://roll.sh)
//   FAKE_ROLL_LOGIN_MS  how long `agent login` waits for "approval" (default 100)
import { appendFileSync, writeFileSync } from "node:fs";

const args = process.argv.slice(2);
if (process.env.FAKE_ROLL_LOG) appendFileSync(process.env.FAKE_ROLL_LOG, `${process.pid} ${args.join(" ")}\n`);
const cmd = args.slice(0, 2).join(" ");

if (cmd === "agent up") {
  console.error("roll agent: fake (m_1) for acme, data in /tmp/fake");
  console.error("roll agent: connected to roll as acme/m_1");
  if (process.env.FAKE_ROLL_IGNORE_TERM) process.on("SIGTERM", () => console.log("ignoring SIGTERM"));
  else process.on("SIGTERM", () => {
    console.log("roll agent: stopping");
    process.exit(0);
  });
  const exit = process.env.FAKE_ROLL_EXIT;
  if (exit !== undefined) setTimeout(() => process.exit(Number(exit)), Number(process.env.FAKE_ROLL_RUN_MS ?? 0));
  else setInterval(() => {}, 1 << 30);
} else if (cmd === "agent login") {
  const uri = `${process.env.FAKE_ROLL_WEB ?? "https://roll.sh"}/dashboard/agents/connect?code=ABCD-EFGH`;
  const json = args.includes("--json");
  if (json) console.log(JSON.stringify({ device_code: "dev_123", user_code: "ABCD-EFGH", verification_uri: uri, interval: 5, expires_in: 900 }));
  else console.log(`To connect fake to roll, open\n\n  ${uri}\n\nand confirm the code ABCD-EFGH`);
  setTimeout(() => {
    if (process.env.FAKE_ROLL_LOGIN === "approve") {
      writeFileSync(process.env.FAKE_ROLL_AGENT_JSON, JSON.stringify({ api: "https://api.roll.sh", machine: "m_1", owner: "acme", name: "fake", token: "rlm_secret" }));
      console.log(json ? JSON.stringify({ status: "approved", machine: "m_1", owner: "acme", name: "fake" }) : "Connected fake to acme as m_1. Run `roll agent up` to start it.");
      process.exit(0);
    }
    console.error("Error: the login was denied");
    process.exit(1);
  }, Number(process.env.FAKE_ROLL_LOGIN_MS ?? 100));
} else {
  console.error(`fake roll: unknown command ${args.join(" ")}`);
  process.exit(2);
}
