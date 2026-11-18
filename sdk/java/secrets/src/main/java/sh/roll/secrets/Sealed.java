package sh.roll.secrets;

import java.util.Map;

/**
 * A {@code roll-seal-v1} box, as crates/roll-secrets/src/crypto.rs makes it (see {@link SealKey}):
 * the version, the sender's ephemeral X25519 public key, the 12-byte nonce and the AES-256-GCM
 * ciphertext with its tag, each base64url without padding.
 */
public record Sealed(int v, String epk, String nonce, String ct) {
    static Sealed of(Map<String, Object> o) {
        if (o == null) {
            return new Sealed(0, "", "", "");
        }
        return new Sealed((int) Json.num(o, "v"), Json.str(o, "epk"), Json.str(o, "nonce"), Json.str(o, "ct"));
    }

    @Override
    public String toString() {
        return "Sealed[v=" + v + "]";
    }
}
