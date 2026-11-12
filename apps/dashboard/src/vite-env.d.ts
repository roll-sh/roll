/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** The rolld API as browsers reach it, built into the client (default `http://127.0.0.1:7070`). */
  readonly VITE_ROLL_API?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
