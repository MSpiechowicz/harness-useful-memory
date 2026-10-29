import path from "node:path";

import { createCommandRunner } from "../command-core.js";
import { runDoctorSetup } from "../doctor-setup.js";

const HELP = "doctor [--check]";

/** Run one subcommand for Claude Code; never throws. */
export const runCommand = createCommandRunner({
  help: HELP,
  projectRoot: () => path.resolve("."),
  runDoctor: runDoctorSetup,
});

if (process.argv[1]?.endsWith("command.js")) {
  console.log(HELP);
}
