import type { PasswordClass, PasswordPolicy } from "./secrets-api";

/** Letters and digits, as the CLI's `roll secrets set KEY --generate N` makes them. */
const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/** The symbols a policy's `symbol` class means unless it says (the server's `SYMBOLS`). */
export const SYMBOLS = "!#%&*+-=?^_~";

const CLASS: Record<PasswordClass, string> = { lower: "abcdefghijklmnopqrstuvwxyz", upper: "ABCDEFGHIJKLMNOPQRSTUVWXYZ", digit: "0123456789", symbol: SYMBOLS };

/** `length` characters of `alphabet`, each equally likely (bytes past the alphabet's last whole multiple are drawn again). */
function draw(alphabet: string, length: number): string {
  const limit = 256 - (256 % alphabet.length);
  let out = "";
  while (out.length < length) {
    for (const b of crypto.getRandomValues(new Uint8Array(length * 2))) {
      if (b < limit && out.length < length) out += alphabet[b % alphabet.length];
    }
  }
  return out;
}

/**
 * A random value made in the browser (never sent anywhere until it's saved): `length` letters and
 * digits, or what the key's password policy asks (its length, one of each of its classes, nothing
 * else).
 */
export function generateValue(length = 48, policy?: PasswordPolicy): string {
  if (!policy) return draw(ALPHABET, length);
  const classes = policy.classes?.length ? policy.classes : (["lower", "upper", "digit"] as PasswordClass[]);
  const sets = classes.map((c) => (c === "symbol" ? (policy.symbols ?? SYMBOLS) : CLASS[c]));
  for (;;) {
    const v = draw(sets.join(""), policy.length);
    if (sets.every((s) => [...v].some((ch) => s.includes(ch)))) return v;
  }
}
