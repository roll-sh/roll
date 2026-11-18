package sh.roll.secrets;

import java.nio.charset.StandardCharsets;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;

/**
 * One of the project's encryption keys (docs/secrets.md, "Encryption keys"): data is encrypted,
 * signed and HMACed by roll, and the key never leaves it. It mirrors {@code @roll/secrets}'
 * {@code transit(name)} and {@code sdk/go/secrets}' {@code Transit}. Strings are sent as UTF-8;
 * {@link #decrypt} answers bytes.
 *
 * <p>Errors are {@link RollSecretsException}s with roll's status and code (404 for no such key,
 * 403 {@code forbidden} for a token that may not use it, 400 for a ciphertext that doesn't open
 * under its context). Neither they nor {@link #toString()} hold a plaintext, a ciphertext, an
 * input or the token.
 */
public final class Transit {
    private final RollSecrets c;
    private final String name;

    Transit(RollSecrets c, String name) {
        this.c = c;
        this.name = name;
    }

    /** The key's name. */
    public String name() {
        return name;
    }

    private String at(String op) {
        return "/v1/secrets/projects/" + RollSecrets.seg(c.projectId()) + "/transit/" + RollSecrets.seg(name) + "/" + op;
    }

    private String post(String op, Map<String, Object> body, String field) {
        String v = Json.str(c.call("POST", at(op), body), field);
        if (v == null) {
            throw new RollSecretsException(0, "invalid", "the answer has no " + field);
        }
        return v;
    }

    private static String b64(byte[] data) {
        return Base64.getEncoder().encodeToString(data);
    }

    private static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    /** A context, or null (left out of the body) when there's none, as the other clients. */
    private static String optional(String s) {
        return s == null || s.isEmpty() ? null : s;
    }

    /**
     * {@code data}'s ciphertext ({@code roll:v<n>:…}) under the key's newest version. A
     * {@code context} (a row's id, say; null or empty for none) is bound in: decrypting needs the
     * same one.
     */
    public String encrypt(byte[] data, String context) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("plaintext", b64(data));
        body.put("context", optional(context));
        return post("encrypt", body, "ciphertext");
    }

    /** {@link #encrypt(byte[], String)} with no context. */
    public String encrypt(byte[] data) {
        return encrypt(data, null);
    }

    /** {@link #encrypt(byte[], String)} of a string's UTF-8. */
    public String encrypt(String data, String context) {
        return encrypt(utf8(data), context);
    }

    /** {@link #encrypt(byte[], String)} of a string's UTF-8, with no context. */
    public String encrypt(String data) {
        return encrypt(utf8(data), null);
    }

    /**
     * The plaintext of {@code ciphertext}, by whichever version made it (a reveal, audited).
     * {@code context} is the one it was encrypted with (null or empty for none).
     */
    public byte[] decrypt(String ciphertext, String context) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("ciphertext", ciphertext);
        body.put("context", optional(context));
        String plain = post("decrypt", body, "plaintext");
        try {
            return Base64.getDecoder().decode(plain);
        } catch (IllegalArgumentException e) {
            // Base64's own message names the bad character: the plaintext's.
            throw new RollSecretsException(0, "invalid", "the answer's plaintext isn't base64");
        }
    }

    /** {@link #decrypt(String, String)} with no context. */
    public byte[] decrypt(String ciphertext) {
        return decrypt(ciphertext, null);
    }

    /**
     * An Ed25519 signature ({@code roll:v<n>:…}) over {@code data} exactly as given, by the newest
     * version: anyone with {@link #publicKeys()} checks it.
     */
    public String sign(byte[] data) {
        return post("sign", Map.of("input", b64(data)), "signature");
    }

    /** {@link #sign(byte[])} of a string's UTF-8. */
    public String sign(String data) {
        return sign(utf8(data));
    }

    /**
     * Whether {@code signature} is the key's over {@code data}. One by a retired or unknown
     * version is false; a malformed one is a 400.
     */
    public boolean verify(byte[] data, String signature) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("input", b64(data));
        body.put("signature", signature);
        Object valid = c.call("POST", at("verify"), body).get("valid");
        if (!(valid instanceof Boolean)) {
            throw new RollSecretsException(0, "invalid", "the answer has no valid");
        }
        return (Boolean) valid;
    }

    /** {@link #verify(byte[], String)} of a string's UTF-8. */
    public boolean verify(String data, String signature) {
        return verify(utf8(data), signature);
    }

    /**
     * HMAC-SHA256 of {@code data} ({@code roll:v<n>:…}), the same for the same input under one
     * version: a lookup column over encrypted fields (a blind index).
     */
    public String hmac(byte[] data) {
        return post("hmac", Map.of("input", b64(data)), "hmac");
    }

    /** {@link #hmac(byte[])} of a string's UTF-8. */
    public String hmac(String data) {
        return hmac(utf8(data));
    }

    /** The key's public signing keys, one per version, oldest first. */
    public List<TransitPublicKey> publicKeys() {
        return c.callList("GET", at("public")).stream().map(TransitPublicKey::of).collect(Collectors.toUnmodifiableList());
    }

    @Override
    public String toString() {
        return "Transit[" + name + "]";
    }
}
