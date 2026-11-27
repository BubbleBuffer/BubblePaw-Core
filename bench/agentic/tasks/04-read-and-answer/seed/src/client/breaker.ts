import type { ClientConfig } from "../config/load.ts";

export class CircuitBreaker {
  private failures = 0;
  private openUntil = 0;

  constructor(private readonly cfg: ClientConfig, private readonly now = () => Date.now()) {}

  canRequest(): boolean {
    return this.now() >= this.openUntil;
  }

  recordSuccess(): void {
    this.failures = 0;
    this.openUntil = 0;
  }

  recordFailure(): void {
    this.failures += 1;
    if (this.failures >= 5) {
      this.openUntil = this.now() + this.cfg.breakerOpenMs;
    }
  }
}
