/** Letters and digits, as the CLI's `roll secrets set KEY --generate N` makes them. */
const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/**
 * A random value of `length` letters and digits, made in the browser (never sent anywhere until
 * it's saved). Bytes at or above the largest multiple of the alphabet's size are drawn again, so
 * every character is equally likely.
 */
export function generateValue(length = 48): string {
  const limit = 256 - (256 % ALPHABET.length);
  let out = "";
  while (out.length < length) {
    for (const b of crypto.getRandomValues(new Uint8Array(length * 2))) {
      if (b < limit && out.length < length) out += ALPHABET[b % ALPHABET.length];
    }
  }
  return out;
}
