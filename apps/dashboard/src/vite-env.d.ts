/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** The rolld API as browsers reach it, built into the client (default `http://127.0.0.1:7070`). */
  readonly VITE_ROLL_API?: string;
  /** A tasks API token, built in for local development. */
  readonly VITE_ROLL_TASKS_TOKEN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
