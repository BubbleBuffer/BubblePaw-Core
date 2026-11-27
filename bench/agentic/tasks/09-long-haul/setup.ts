// Task setup hook: writes the deterministic receiving notes into the workspace (argv[2]).
// Runs outside the workspace, so the generator and the truth never reach the agent.
import { writeMaterial } from "./render.ts";

const workspace = Bun.argv[2];
if (!workspace) {
  throw new Error("Usage: setup.ts <workspace>");
}
await writeMaterial(workspace);
