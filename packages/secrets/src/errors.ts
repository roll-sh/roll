/** An answer that isn't 2xx: the API's `error`, its `code` (`approval_required`, `conflict`…) and the status. */
export class SecretsError extends Error {
  override readonly name = "SecretsError";
  readonly status: number;
  readonly code: string | null;

  constructor(message: string, status: number, code: string | null = null) {
    super(message);
    this.status = status;
    this.code = code;
  }
}
