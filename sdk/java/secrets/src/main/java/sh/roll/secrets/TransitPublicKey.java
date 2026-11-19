package sh.roll.secrets;

import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * One version of an encryption key's public signing key (Ed25519): raw base64, PEM (SPKI) and a
 * JWK whose {@code kid} is {@code <name>:v<version>}. Signatures by that version check against it
 * with any Ed25519 library.
 */
public record TransitPublicKey(int version, String publicKey, String pem, Map<String, String> jwk) {
    static TransitPublicKey of(Map<String, Object> o) {
        Map<String, String> jwk = new LinkedHashMap<>();
        Map<String, Object> raw = Json.obj(o, "jwk");
        if (raw != null) {
            raw.forEach((k, v) -> {
                if (v instanceof String) {
                    jwk.put(k, (String) v);
                }
            });
        }
        return new TransitPublicKey((int) Json.num(o, "version"), Json.str(o, "public_key"), Json.str(o, "pem"),
                Collections.unmodifiableMap(jwk));
    }
}
