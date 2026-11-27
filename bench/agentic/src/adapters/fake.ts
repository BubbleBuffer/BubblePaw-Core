import type { AgentAdapter, AgentRunRequest, AgentRunResult } from "../types.ts";
import { applyFakeSolution } from "./fake-solutions.ts";

export type FakeMode = "solve" | "noop";

export class FakeAdapter implements AgentAdapter {
  readonly name: string;
  private readonly mode: FakeMode;

  constructor(mode: FakeMode = "solve") {
    this.mode = mode;
    this.name = mode === "solve" ? "fake" : "fake-noop";
  }

  async version(): Promise<string | null> {
    return "fake-1.0.0";
  }

  async run(req: AgentRunRequest): Promise<AgentRunResult> {
    const startedAt = performance.now();
    if (this.mode === "noop") {
      return {
        finalText: "No changes made.",
        wallMs: Math.max(1, Math.round(performance.now() - startedAt)),
        firstTextMs: 1,
        requests: 1,
        inputTokens: 120,
        outputTokens: 12,
        cachedTokens: 0,
        cacheCreationTokens: 0,
        reasoningTokens: null,
        costUsd: 0,
        costSource: "agent",
        toolCalls: 0,
        toolErrors: 0,
        exitCode: 0,
        error: null,
        timedOut: false,
        unexplainedCacheMisses: 0,
        repeatedToolCalls: 0,
        compactions: 0,
        peakRequestInputTokens: 120,
      };
    }

    const summary = await applyFakeSolution(req.taskId, req.workspace);
    const wallMs = Math.max(1, Math.round(performance.now() - startedAt));
    return {
      finalText: summary.text,
      wallMs,
      firstTextMs: Math.min(wallMs, 2),
      requests: summary.requests,
      inputTokens: 400,
      outputTokens: 150,
      cachedTokens: 100,
      cacheCreationTokens: 0,
      reasoningTokens: null,
      costUsd: 0,
      costSource: "agent",
      toolCalls: summary.toolCalls,
      toolErrors: 0,
      exitCode: 0,
      error: null,
      timedOut: false,
      unexplainedCacheMisses: 0,
      repeatedToolCalls: 0,
      compactions: 0,
      peakRequestInputTokens: 400,
    };
  }
}
