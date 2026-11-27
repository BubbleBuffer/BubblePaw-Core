import type { AdapterConfig, AgentAdapter } from "../types.ts";
import { BubblePawAdapter } from "./bubblepaw.ts";
import { FakeAdapter } from "./fake.ts";
import { PiAdapter } from "./pi.ts";

export { BubblePawAdapter, FakeAdapter, PiAdapter };

export function createAdapter(name: string, config: AdapterConfig = {}): AgentAdapter {
  switch (name) {
    case "bubblepaw":
      return new BubblePawAdapter(config);
    case "pi":
      return new PiAdapter(config);
    case "fake":
      return new FakeAdapter("solve");
    case "fake-noop":
      return new FakeAdapter("noop");
    default:
      throw new Error(
        `Unknown agent adapter "${name}". Expected one of: bubblepaw, pi, fake, fake-noop.`,
      );
  }
}
