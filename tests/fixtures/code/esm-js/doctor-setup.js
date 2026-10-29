import { execFile } from "node:child_process";

const PROBE_TERM_MS = 2000;

/** Whether Harness Memory offers the code index. */
export function probeCodeGraph(execute = execFile, { cwd, signal } = {}) {
  return new Promise(done => {
    execute("harness-memory", ["features"], { cwd, signal, timeout: PROBE_TERM_MS }, (error, stdout) => {
      done(error ? "harness-memory not found" : stdout);
    });
  });
}

/** The services doctor uses; a caller may replace any of them. */
function doctorServices(services = {}) {
  return {
    probeCodeGraph: services.probeCodeGraph ?? probeCodeGraph,
  };
}

async function codeGraphStep({ services, cwd, signal }) {
  return services.probeCodeGraph({ cwd, signal });
}

/** Doctor memory setup shared by host adapters. */
export async function runDoctorSetup({ cwd, check, services } = {}) {
  const code = await codeGraphStep({ services: doctorServices(services), cwd });
  return { code, check };
}
