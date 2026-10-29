/** Binds the subcommands to one host profile; `host.runDoctor` is the doctor setup the host passes in. */
export function createCommandRunner(host) {
  async function doctor(tokens, { cwd, check, doctorServices }) {
    const outcome = await host.runDoctor({ cwd, check, services: doctorServices });
    return outcome.code;
  }

  async function status() {
    return "ok";
  }

  return async function runCommand(argv, context) {
    return argv[0] === "doctor" ? doctor(argv, context) : status();
  };
}
