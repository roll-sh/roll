export { createClient } from "./client.js";
export type { ClientOptions, SecretsClient, WatchOptions } from "./client.js";
export { SecretsError } from "./errors.js";
export { generateKeyPair, openSealed, publicKey, sealTo } from "./seal.js";
export type { KeyPair } from "./seal.js";
export type * from "./types.js";
