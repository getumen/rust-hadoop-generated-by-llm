import http.server
import json
import base64
import time
import os
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa
import jwt # pip install pyjwt cryptography

PORT = int(os.environ.get("OIDC_PORT", 8080))
CLIENT_ID = os.environ.get("OIDC_CLIENT_ID", "test-client")
# Use environment variable for issuer, default to localhost for local testing
ISSUER = os.environ.get("OIDC_ISSUER", f"http://localhost:{PORT}")

# Generate RSA key for the mock OIDC provider
private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
public_key = private_key.public_key()

# Construct JWKS
def get_jwks():
    numbers = public_key.public_numbers()
    def to_base64_url(n):
        return base64.urlsafe_b64encode(n.to_bytes((n.bit_length() + 7) // 8, 'big')).decode('utf-8').rstrip('=')

    return {
        "keys": [
            {
                "kty": "RSA",
                "kid": "test-kid",
                "use": "sig",
                "alg": "RS256",
                "n": to_base64_url(numbers.n),
                "e": to_base64_url(numbers.e)
            }
        ]
    }

class MockOidcHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/.well-known/openid-configuration":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({
                "issuer": ISSUER,
                "jwks_uri": f"{ISSUER}/jwks"
            }).encode())
        elif self.path == "/jwks":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(get_jwks()).encode())
        elif self.path == "/token":
            # Generate token endpoint for automated tests
            sub = os.environ.get("OIDC_SUB", "user-123")
            groups = os.environ.get("OIDC_GROUPS", "tenant-a").split(',')
            token = generate_token(sub, groups)
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            # jwt.encode returns bytes in some versions, str in others
            if isinstance(token, str):
                self.wfile.write(token.encode())
            else:
                self.wfile.write(token)
        else:
            self.send_response(404)
            self.end_headers()

def generate_token(sub, groups):
    payload = {
        "sub": sub,
        "aud": CLIENT_ID,
        "iss": ISSUER,
        "exp": int(time.time()) + 3600,
        "iat": int(time.time()),
        "groups": groups
    }
    return jwt.encode(payload, private_key, algorithm="RS256", headers={"kid": "test-kid"})

if __name__ == "__main__":
    import threading
    import sys

    if len(sys.argv) > 1 and sys.argv[1] == "token":
        # Generate token and print it
        sub = os.environ.get("OIDC_SUB", "user-123")
        groups = os.environ.get("OIDC_GROUPS", "tenant-a").split(',')
        token = generate_token(sub, groups)
        if isinstance(token, bytes):
            token = token.decode('utf-8')
        print(token)
        sys.exit(0)

    # Listen on 0.0.0.0 to be accessible within Docker networks
    server = http.server.HTTPServer(('0.0.0.0', PORT), MockOidcHandler)
    print(f"Mock OIDC server started on {ISSUER} (listening on 0.0.0.0:{PORT})")
    server.serve_forever()
