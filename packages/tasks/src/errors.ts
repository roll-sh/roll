/** An error response from the roll tasks API (`{ "error": string }` with a 4xx/5xx status). */
export class TasksError extends Error {
  override readonly name = "TasksError";
  /** HTTP status code. */
  readonly status: number;
  /** The parsed response body, when there was one. */
  readonly body: unknown;

  constructor(status: number, message: string, body?: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

export type SignatureFailure = "missing" | "expired" | "mismatch" | "invalid";

/** A request whose Standard Webhooks signature doesn't verify. */
export class SignatureError extends Error {
  override readonly name = "SignatureError";
  readonly reason: SignatureFailure;

  constructor(reason: SignatureFailure, message: string) {
    super(message);
    this.reason = reason;
  }
}

/**
 * Throw from a function or a step to fail without retries. From a step, the error is memoised
 * at once (and thrown as a {@link StepError} on the next call); from the function, the run fails.
 */
export class NonRetriableError extends Error {
  override readonly name: string = "NonRetriableError";

  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options);
  }
}

/** The shape errors take on the wire. */
export interface SerializedError {
  name: string;
  message: string;
  stack?: string;
  [key: string]: unknown;
}

/**
 * A step that failed for good: its retries ran out, it threw a {@link NonRetriableError}, or the
 * function it invoked failed. Catch it to handle the failure; left uncaught it fails the run.
 */
export class StepError extends Error {
  override readonly name = "StepError";
  /** The step's id. */
  readonly stepId: string;
  /** The error as roll recorded it (`name`, `message`, …). */
  readonly error: SerializedError;

  constructor(stepId: string, error: unknown) {
    const e = normalizeError(error);
    super(e.message);
    this.stepId = stepId;
    this.error = e;
  }
}

export function normalizeError(error: unknown): SerializedError {
  if (error && typeof error === "object" && !Array.isArray(error)) {
    const o = error as Record<string, unknown>;
    return { ...o, name: typeof o.name === "string" ? o.name : "Error", message: typeof o.message === "string" ? o.message : JSON.stringify(o) };
  }
  if (typeof error === "string") return { name: "Error", message: error };
  return { name: "Error", message: String(error) };
}

/** Turn anything thrown into the wire shape. */
export function serializeError(error: unknown): SerializedError {
  if (error instanceof StepError) {
    return { name: error.error.name, message: error.error.message, ...(error.stack ? { stack: error.stack } : {}), step: error.stepId };
  }
  if (error instanceof Error) {
    const out: SerializedError = { name: error.name || "Error", message: error.message };
    if (error.stack) out.stack = error.stack;
    return out;
  }
  return normalizeError(error);
}
