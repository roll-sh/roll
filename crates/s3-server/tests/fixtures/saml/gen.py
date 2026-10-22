"""Generate a SAML response signed by signxml (lxml's exclusive C14N, xmlsec-compatible), as an
interop fixture for roll's verifier. Writes response.xml (the signed document, not base64) and
metadata.xml into /out."""
import datetime

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID
from lxml import etree
from signxml import XMLSigner, methods

ACCOUNT = "123456789012"
ROLE = f"arn:aws:iam::{ACCOUNT}:role/interop"
PROVIDER = f"arn:aws:iam::{ACCOUNT}:saml-provider/signxml"
ISSUER = "https://idp.signxml.test/metadata"

key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "signxml interop IdP")])
cert = (
    x509.CertificateBuilder()
    .subject_name(name)
    .issuer_name(name)
    .public_key(key.public_key())
    .serial_number(1)
    .not_valid_before(datetime.datetime(2024, 1, 1))
    .not_valid_after(datetime.datetime(2099, 1, 1))
    .sign(key, hashes.SHA256())
)
cert_pem = cert.public_bytes(serialization.Encoding.PEM)
cert_b64 = "".join(cert_pem.decode().splitlines()[1:-1])

# Indented, with namespaces declared on the Response only and a default namespace on the
# AttributeStatement: exclusive canonicalization has real work to do.
response = f"""<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" ID="_resp1" Version="2.0" IssueInstant="2024-06-01T00:00:00Z" Destination="https://signin.aws.amazon.com/saml">
  <saml:Issuer>{ISSUER}</saml:Issuer>
  <samlp:Status>
    <samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/>
  </samlp:Status>
  <saml:Assertion ID="_assert1" Version="2.0" IssueInstant="2024-06-01T00:00:00Z">
    <saml:Issuer>{ISSUER}</saml:Issuer>
    <saml:Subject>
      <saml:NameID Format="urn:oasis:names:tc:SAML:2.0:nameid-format:persistent">interop-user@example.com</saml:NameID>
      <saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer">
        <saml:SubjectConfirmationData NotOnOrAfter="2099-01-01T00:00:00Z" Recipient="https://signin.aws.amazon.com/saml"/>
      </saml:SubjectConfirmation>
    </saml:Subject>
    <saml:Conditions NotBefore="2024-01-01T00:00:00Z" NotOnOrAfter="2099-01-01T00:00:00Z">
      <saml:AudienceRestriction>
        <saml:Audience>https://signin.aws.amazon.com/saml</saml:Audience>
      </saml:AudienceRestriction>
    </saml:Conditions>
    <saml:AuthnStatement AuthnInstant="2024-06-01T00:00:00Z">
      <saml:AuthnContext>
        <saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef>
      </saml:AuthnContext>
    </saml:AuthnStatement>
    <saml:AttributeStatement>
      <saml:Attribute Name="https://aws.amazon.com/SAML/Attributes/Role">
        <saml:AttributeValue xsi:type="xs:string">{ROLE},{PROVIDER}</saml:AttributeValue>
      </saml:Attribute>
      <saml:Attribute Name="https://aws.amazon.com/SAML/Attributes/RoleSessionName">
        <saml:AttributeValue xsi:type="xs:string">interop-user</saml:AttributeValue>
      </saml:Attribute>
      <saml:Attribute Name="urn:oid:0.9.2342.19200300.100.1.3">
        <saml:AttributeValue xsi:type="xs:string">interop-user@example.com</saml:AttributeValue>
      </saml:Attribute>
    </saml:AttributeStatement>
  </saml:Assertion>
</samlp:Response>"""

doc = etree.fromstring(response.encode())
assertion = doc.find("{urn:oasis:names:tc:SAML:2.0:assertion}Assertion")
signer = XMLSigner(method=methods.enveloped, signature_algorithm="rsa-sha256", digest_algorithm="sha256", c14n_algorithm="http://www.w3.org/2001/10/xml-exc-c14n#")
signed_assertion = signer.sign(assertion, key=key, cert=cert_pem.decode(), reference_uri="_assert1")
# signxml appends the Signature at the end; SAML wants it after Issuer.
sig = signed_assertion.find("{http://www.w3.org/2000/09/xmldsig#}Signature")
signed_assertion.remove(sig)
signed_assertion.insert(1, sig)
doc.replace(assertion, signed_assertion)
out = etree.tostring(doc, xml_declaration=False).decode()

# Check it with signxml itself before writing it out.
from signxml import XMLVerifier

XMLVerifier().verify(out.encode(), x509_cert=cert_pem.decode(), expect_references=1)

metadata = f"""<?xml version="1.0"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" xmlns:ds="http://www.w3.org/2000/09/xmldsig#" entityID="{ISSUER}">
  <md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">
    <md:KeyDescriptor use="signing">
      <ds:KeyInfo>
        <ds:X509Data>
          <ds:X509Certificate>{cert_b64}</ds:X509Certificate>
        </ds:X509Data>
      </ds:KeyInfo>
    </md:KeyDescriptor>
    <md:NameIDFormat>urn:oasis:names:tc:SAML:2.0:nameid-format:persistent</md:NameIDFormat>
    <md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="https://idp.signxml.test/sso"/>
  </md:IDPSSODescriptor>
</md:EntityDescriptor>
"""
open("/out/response.xml", "w").write(out)
open("/out/metadata.xml", "w").write(metadata)
print("ok", len(out), len(metadata))
